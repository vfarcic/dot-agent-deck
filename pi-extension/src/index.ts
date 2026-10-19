/**
 * dot-agent-deck orchestrator extension for Pi (PRD #201, M2.1 + M2.2).
 *
 * This is the thin Pi-API glue. It:
 *   1. registers `delegate` and `work_done` as native, schema-validated tools
 *      whose bodies shell the `dot-agent-deck` CLI,
 *   2. runs the deck's inbox receiver (`./inbox-receiver.js`, PRD #1666), one
 *      per Pi process, and hands each deck message to Pi behind the deck's
 *      final check, and
 *   3. subscribes to Pi's event bus and reports the pane's status
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
	INBOX_ADAPTER_INTEGRATION,
	inboxBinary,
	inboxKey,
	inboxReceiver,
	inboxRef,
	type InboxApproval,
	type InboxMessage as InboxReceiverMessage,
	type InboxReceiver,
	type InboxReceiverState,
	type InboxRef,
} from "./inbox-receiver.js";
import {
	buildDelegateArgv,
	buildInboxAnnounceArgv,
	buildInboxDoorbellFetchArgv,
	buildWorkDoneArgv,
	createReporter,
	createSerialQueue,
	createTurnReplyTracker,
	DeckExecError,
	doorbellIdsIn,
	execFailureMessage,
	execWithStdin,
	inboxIdsIn,
	inboxIdsToAck,
	type InboxMessage,
	inboxUserText,
	installQuestionWrappers,
	isAbsoluteOverride,
	parseInboxFetch,
	piEventReport,
	resolveDeckBin,
	spawnFailureMessage,
	tagHandOver,
	untagHandOver,
	WATCHER_NOT_IN_DECK_EXIT,
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

// --- PRD #1666: the deck's inbox -------------------------------------------

/**
 * The deck binary the receiver runs its watcher with: the resolved CLI when it
 * is an absolute path, else none, so the receiver never runs a bare name from
 * Pi's `$PATH` (`resolveDeckBin`'s fallback for an older deck). The generated
 * module refuses anything but an absolute path to an executable file too.
 */
inboxBinary(() => (isAbsoluteOverride(deckBin, process.platform) ? deckBin : undefined));

/**
 * Bumped whenever this file's inbox code changes: a reload that evaluates a
 * different version replaces the process's receiver (`replaceVersion`).
 */
export const INBOX_VERSION = 2;
/**
 * The first receipt version a release carries. Version 1 was never released,
 * so a receiver older than this one is replaced as incompatible: nothing is
 * carried from it (`carriesState`).
 */
const FIRST_RECEIPT_VERSION = 2;
/**
 * The shape of what a replacement carries over (`Transfer`). Bumped only when
 * that shape changes; a replacement across a different one carries nothing.
 */
const INBOX_STATE_VERSION = 1;
/** How long a doorbell waits for the receiver's watcher to register before it consumes the line and leaves the mail queued. */
export const DOORBELL_REGISTER_WAIT_MS = 5000;
/**
 * How long a hand-over may go unseen by this extension's `input` gate while Pi
 * sits idle with nothing queued before it counts as dropped: positively not
 * handed, since nothing past the gate saw it.
 */
export const INBOX_UNSEEN_RECHECK_MS = 30000;
/** How many committed attempts and hand-over tags the process remembers. */
const INBOX_REMEMBERED = 4096;

/** An attempt handed to `sendUserMessage` whose hand-over the `input` gate has not seen. */
interface Sent {
	message: InboxMessage;
	ref: InboxRef;
	tag: string;
	/** The session instance whose `pi` made the call. */
	token: number;
	granted: InboxApproval | undefined;
	/** `runs` when it was sent. */
	run: number;
}

/** An attempt past a native hand-over point (the gate's `transform`, or a doorbell's): possibly handed. */
interface Passed {
	ref: InboxRef;
	token: number;
	/** `input` once past the gate; `start` once a `before_agent_start` prompt carried it. */
	stage: "input" | "start";
	/** `runs` when it passed. */
	run: number;
	/** At `input`: seen waiting in Pi's queue behind the run active when it passed (`watchQueue`). */
	queued: boolean;
	/** At `start`: a settle came while another run was active, the way Pi settles a prompt it refused. */
	suspect: boolean;
	/** Released `lost`: it waits for a commit only. */
	lost: boolean;
}

/** The session instance currently receiving deck messages. */
interface Current {
	token: number;
	pi: ExtensionAPI;
	ctx: ExtensionContext | undefined;
}

/**
 * One receiver per Pi process (PRD #1666 §0): every session instance, `/new`,
 * resume, fork and reload included, shares it, its watcher and the hand-over
 * gate's records, so a commit seen in a successor is published by the
 * receiver that holds its authority. Kept on `globalThis` under
 * `INBOX_PROCESS` so a reload that evaluates this module again finds it.
 */
interface InboxProcess {
	version: number;
	stateVersion: number;
	receiver: InboxReceiver;
	sent: Map<string, Sent>;
	/** Hand-over tag → attempt key, bounded. */
	tags: Map<string, string>;
	passed: Map<string, Passed>;
	/** Message id → its attempt keys. */
	byId: Map<string, Set<string>>;
	/** Attempt keys committed, bounded. */
	committed: Set<string>;
	/** Attempts held while no session is current, in order. */
	pending: InboxMessage[];
	current: Current | undefined;
	nextToken: number;
	nextTag: number;
	tagPrefix: string;
	/** Runs Pi started (`agent_start`), to date hand-overs. */
	runs: number;
	/** `pending` is being handed over, one attempt at a time, oldest first (`pump`). */
	pumping: boolean;
	/** Every status report, across instances, in the order the handlers queued them. */
	reportQueue: <T>(task: () => Promise<T>) => Promise<T>;
	rechecks: Map<string, ReturnType<typeof setTimeout>>;
	/** The instance a `reload` shut down, whose passed attempts its successor takes over. */
	reloadedFrom: number | undefined;
	/** A version replacement under way: later instances wait for it. */
	replacing: Promise<void> | undefined;
	disposed: boolean;
	/** The watcher said this process runs outside a deck pane: no respawn. */
	notInDeck: boolean;
}

/** What a replacement carries from a receiver of the same state version. */
interface Transfer {
	receiver: unknown;
	sent: Map<string, Sent>;
	tags: Map<string, string>;
	passed: Map<string, Passed>;
	byId: Map<string, Set<string>>;
	committed: Set<string>;
	pending: InboxMessage[];
	nextToken: number;
	nextTag: number;
	tagPrefix: string;
	runs: number;
	reloadedFrom: number | undefined;
	reportQueue: InboxProcess["reportQueue"] | undefined;
}

const INBOX_PROCESS = Symbol.for("dot-agent-deck.pi-receiver");
/** The process state an extension from before the receipt stream kept. */
const LEGACY_INBOX = Symbol.for("dot-agent-deck.pi-inbox");
const HANDLED = { action: "handled" } as const;
type InputAnswer = typeof HANDLED | { action: "transform"; text: string } | undefined;

const holder = globalThis as unknown as Record<symbol, unknown>;

/** Remember `key` in a bounded insertion-ordered set. */
function rememberKey(set: Set<string>, key: string): void {
	set.delete(key);
	set.add(key);
	while (set.size > INBOX_REMEMBERED) {
		set.delete(set.values().next().value as string);
	}
}

/** Whether this process committed some attempt of deck message `id` (`proc.committed`). */
function committedMessage(proc: InboxProcess, id: string): boolean {
	const prefix = id + "|";
	for (const key of proc.committed) {
		if (key.startsWith(prefix)) {
			return true;
		}
	}
	return false;
}

/**
 * PR #1735 review 4306950747: an attempt of a message this process already
 * committed — that same attempt again, or a new one the deck issued before it
 * read the commit (a lost or late receipt) — is not handed to Pi a second
 * time. It is reported committed instead, so its watcher publishes the
 * receipt and the deck settles the message rather than keeping the attempt
 * held or repeating it.
 */
function alreadyCommitted(proc: InboxProcess, message: InboxMessage): boolean {
	const ref = inboxRef(message as InboxRef);
	const key = inboxKey(ref);
	if (!proc.committed.has(key) && !committedMessage(proc, message.id)) {
		return false;
	}
	rememberKey(proc.committed, key);
	proc.receiver.committed([ref]);
	return true;
}

function rememberTag(proc: InboxProcess, tag: string, key: string): void {
	proc.tags.set(tag, key);
	while (proc.tags.size > INBOX_REMEMBERED) {
		proc.tags.delete(proc.tags.keys().next().value as string);
	}
}

function noteId(proc: InboxProcess, ref: InboxRef): void {
	const keys = proc.byId.get(ref.id) ?? new Set<string>();
	keys.add(inboxKey(ref));
	proc.byId.set(ref.id, keys);
}

/** Whether the receiver still holds `ref` (a revoke answered or a commit took it otherwise). */
function holds(proc: InboxProcess, ref: InboxRef): boolean {
	return proc.receiver.of(ref.id).some((held) => held.a === ref.a && held.e === ref.e);
}

/** Positively not handed: nothing past a native hand-over point saw it. Silent for an attempt no longer held. */
function refuse(proc: InboxProcess, ref: InboxRef): void {
	clearRecheck(proc, inboxKey(ref));
	if (holds(proc, ref)) {
		proc.receiver.notHanded(ref);
	}
}

function clearRecheck(proc: InboxProcess, key: string): void {
	const timer = proc.rechecks.get(key);
	if (timer !== undefined) {
		clearTimeout(timer);
		proc.rechecks.delete(key);
	}
}

/** Pi's answers, or `undefined` when it cannot be asked: never taken as a reason to release anything. */
function askIdle(ctx: ExtensionContext | undefined): boolean | undefined {
	try {
		return ctx?.isIdle();
	} catch {
		return undefined;
	}
}
function askQueued(ctx: ExtensionContext | undefined): boolean | undefined {
	try {
		return ctx?.hasPendingMessages?.();
	} catch {
		return undefined;
	}
}
/** An agent run is active: Pi's `signal` is set only while one is. */
function askRunning(ctx: ExtensionContext | undefined): boolean | undefined {
	try {
		return ctx === undefined ? undefined : ctx.signal !== undefined;
	} catch {
		return undefined;
	}
}

/**
 * A sent attempt the gate has not seen, re-checked every
 * `INBOX_UNSEEN_RECHECK_MS`: once Pi is idle with nothing queued it never
 * reached the gate, so it is positively not handed, and its late arrival at
 * the gate is consumed.
 */
function armRecheck(proc: InboxProcess, key: string): void {
	clearRecheck(proc, key);
	const timer = setTimeout(() => {
		proc.rechecks.delete(key);
		try {
			const sent = proc.sent.get(key);
			if (sent === undefined || proc.disposed) {
				return;
			}
			const ctx = proc.current?.ctx;
			if (askIdle(ctx) === true && askQueued(ctx) === false) {
				proc.sent.delete(key);
				refuse(proc, sent.ref);
			} else {
				armRecheck(proc, key);
			}
		} catch {
			// Never let a timer callback throw into Pi.
		}
	}, INBOX_UNSEEN_RECHECK_MS);
	(timer as { unref?: () => void }).unref?.();
	proc.rechecks.set(key, timer);
}

/**
 * The receiver handed out `message`: it joins `pending`, which is handed to
 * the current session in order, or kept until one starts. A disposed process
 * keeps it there for the replacement that carries `pending` (§0.5).
 */
function deliver(proc: InboxProcess, message: InboxMessage): void {
	const key = inboxKey(message as InboxRef);
	if (alreadyCommitted(proc, message)) {
		return;
	}
	if (proc.sent.has(key) || proc.passed.has(key)) {
		// Its hand-over is under way.
		return;
	}
	if (proc.pending.some((held) => inboxKey(held as InboxRef) === key)) {
		return;
	}
	proc.pending.push(message);
	pump(proc);
}

/**
 * Hand `pending` to the current session, oldest first, one attempt at a time.
 * It stops while nothing can be handed over — the process disposed, no
 * session current, no watcher registered — and a session start or a
 * registration starts it again. The attempt in hand stays at the head of
 * `pending` until it is sent or refused, so a session swap, a watcher restart
 * or a version replacement during its approval finds it there, uncalled.
 */
function pump(proc: InboxProcess): void {
	if (proc.pumping) {
		return;
	}
	proc.pumping = true;
	void (async () => {
		try {
			while (!proc.disposed && proc.current !== undefined && proc.receiver.rx() !== undefined && proc.pending.length > 0) {
				const message = proc.pending[0];
				if (await handOver(proc, message)) {
					const at = proc.pending.indexOf(message);
					if (at >= 0) {
						proc.pending.splice(at, 1);
					}
				}
			}
		} catch {
			// Never let a hand-over throw into Pi.
		} finally {
			proc.pumping = false;
		}
	})();
}

/**
 * Hand one attempt to the current session behind the final hand-over check:
 * the deck's approval, then — with no await before `sendUserMessage` — the
 * session that asked is still current, the process is not disposed, the
 * watcher that gave the approval still runs, and the receiver's own check.
 * Whether the attempt is done with: sent, or refused (a positive not-handed).
 * One asked of a session, a watcher or a process that is gone was not
 * refused: it stays in `pending`, uncalled, for a fresh check.
 */
async function handOver(proc: InboxProcess, message: InboxMessage): Promise<boolean> {
	const ref = inboxRef(message as InboxRef);
	const key = inboxKey(ref);
	const token = proc.current?.token;
	const epoch = proc.receiver.epoch();
	if (token === undefined) {
		return false;
	}
	const granted = await proc.receiver.may(ref);
	// No await from here to the call.
	const current = proc.current;
	if (proc.disposed || current?.token !== token || proc.receiver.epoch() !== epoch) {
		return false;
	}
	let ready = false;
	try {
		ready = proc.receiver.ready(ref, granted) && !proc.sent.has(key) && !proc.passed.has(key);
	} catch {
		ready = false;
	}
	if (!ready) {
		refuse(proc, ref);
		return true;
	}
	proc.nextTag += 1;
	const tag = `${proc.tagPrefix}.${proc.nextTag}`;
	// Pi can run the `input` gate before `sendUserMessage` returns, so the
	// attempt is on record first.
	proc.sent.set(key, { message, ref, tag, token, granted, run: proc.runs });
	rememberTag(proc, tag, key);
	noteId(proc, ref);
	try {
		current.pi.sendUserMessage(tagHandOver(tag, inboxUserText([message])), { deliverAs: "followUp" });
	} catch {
		// A synchronous throw (a stale `pi`): nothing reached Pi.
		proc.sent.delete(key);
		refuse(proc, ref);
		return true;
	}
	if (proc.sent.has(key)) {
		armRecheck(proc, key);
	}
	return true;
}

/**
 * The native gate (PRD #1666 §2.2): our hand-over reaching Pi's `input`
 * hooks, run by the instance `token`. It passes — `transform`, the hand-over
 * point — only for an attempt this instance sent, while this instance is
 * current and nothing is disposed, and only while the receiver still
 * authorizes it with the approval the call was made under. Anything else is
 * consumed (`handled`), never passed through: an unknown or older tag
 * silently; one that failed a check after a positive not-handed when the
 * receiver still holds it.
 */
function gate(proc: InboxProcess, instance: Instance, event: unknown, ctx: ExtensionContext | undefined): InputAnswer {
	const input = event as { text?: unknown; source?: unknown } | undefined;
	if (input?.source !== "extension") {
		return undefined;
	}
	const tagged = untagHandOver(input.text);
	if (!tagged) {
		return undefined;
	}
	const key = proc.tags.get(tagged.attempt);
	const sent = key === undefined ? undefined : proc.sent.get(key);
	if (key === undefined || sent === undefined || sent.tag !== tagged.attempt) {
		return HANDLED;
	}
	proc.sent.delete(key);
	clearRecheck(proc, key);
	const live =
		!proc.disposed &&
		!instance.shutdown &&
		instance.token !== undefined &&
		proc.current?.token === instance.token &&
		sent.token === instance.token;
	let ready = false;
	try {
		ready = live && proc.receiver.ready(sent.ref, sent.granted);
	} catch {
		ready = false;
	}
	if (!ready) {
		refuse(proc, sent.ref);
		return HANDLED;
	}
	proc.receiver.calling(sent.ref);
	proc.passed.set(key, { ref: sent.ref, token: instance.token!, stage: "input", run: proc.runs, queued: false, suspect: false, lost: false });
	watchQueue(proc, key, ctx);
	proc.receiver.handed([sent.ref]);
	return { action: "transform", text: tagged.text };
}

/**
 * How many microtasks after a hand-over point `watchQueue` keeps looking for
 * Pi's choice before it leaves the rest to one macrotask check. Pi 0.84.4
 * queues in the microtasks after the gate returns: a few for its own awaits,
 * and up to about three for each `input` handler that runs after ours and
 * returns without waiting on anything else, so this covers some three hundred
 * of those.
 */
const QUEUE_WATCH_MICROTASKS = 1000;

/**
 * An attempt that passed a hand-over point while a run was active goes into
 * Pi's queue behind that run — unless the run ended in the moment before Pi
 * looked, when it goes on to start a run of its own, through a preflight that
 * reads idle with nothing queued. Pi chooses in the microtasks after the
 * gate returns, unless another extension's `input` hook makes it wait, so the
 * attempt is watched in those microtasks — before any terminal input, so
 * before an Escape or a dequeue can empty the queue — and once more at the
 * next macrotask, for a hook that made Pi wait. Still at `input`, with that
 * run still active and something queued, it is marked `queued`: a settle of
 * that run leaving it uncommitted with nothing queued means Pi dropped it from
 * the queue (Escape, or a dequeue to the editor), and `settled` releases it.
 * Unmarked, it stays held until a run starts, as one does when such a hook
 * delays Pi's choice and an Escape empties the queue before the macrotask
 * check. The mark can be wrong when such a hook delays Pi's choice while something else
 * is queued and the run then ends before Pi chooses: the attempt is released
 * `lost` and still runs, which is possibly handed, so the deck's checked,
 * marked repeat covers it.
 */
function watchQueue(proc: InboxProcess, key: string, ctx: ExtensionContext | undefined): void {
	if (askIdle(ctx) !== false) {
		return;
	}
	const run = proc.runs;
	/**
	 * `true` while the attempt is still eligible and not yet seen queued, so
	 * the caller keeps watching; `false` once it is marked queued or there is
	 * nothing left to watch.
	 */
	const look = (): boolean => {
		const passed = proc.passed.get(key);
		if (passed === undefined || passed.stage !== "input" || passed.queued || proc.runs !== run || askIdle(ctx) !== false) {
			return false;
		}
		if (askQueued(ctx) === true) {
			passed.queued = true;
			return false;
		}
		return true;
	};
	let left = QUEUE_WATCH_MICROTASKS;
	const step = (): void => {
		try {
			if (look() && --left > 0) {
				queueMicrotask(step);
			}
		} catch {
			// Never let a callback throw into Pi.
		}
	};
	queueMicrotask(step);
	const timer = setTimeout(() => {
		try {
			look();
		} catch {
			// Never let a timer callback throw into Pi.
		}
	}, 0);
	(timer as { unref?: () => void }).unref?.();
}

/**
 * A submitted input that is nothing but the deck's doorbell (PRD #1666 §2.5).
 * Waits up to `DOORBELL_REGISTER_WAIT_MS` for the watcher to register, then
 * fetches what it announced, issued to this receiver. The fetched attempts are
 * held whatever happens next; the input is transformed in place into their
 * content only if — with no await before the return — the session that
 * fetched is still current, nothing is disposed, the watcher was not
 * restarted, and the receiver authorizes every one under the approval taken
 * before the fetches, which run at once. Otherwise the line is consumed and each is positively not
 * handed. With no registration, or a deck that cannot be asked, the line is
 * consumed and the mail stays queued for the watcher.
 */
async function doorbell(proc: InboxProcess, instance: Instance, ids: string[], ctx: ExtensionContext): Promise<InputAnswer> {
	const until = Date.now() + DOORBELL_REGISTER_WAIT_MS;
	while (proc.receiver.rx() === undefined && Date.now() < until && !proc.disposed) {
		await new Promise((resolve) => setTimeout(resolve, 100));
	}
	const rx = proc.receiver.rx();
	const current = proc.current;
	if (rx === undefined || current === undefined || current.token !== instance.token) {
		return HANDLED;
	}
	const token = current.token;
	const epoch = proc.receiver.epoch();
	const approval = proc.receiver.approval();
	// The ids are fetched at once, so the approval taken before them ages by
	// the slowest fetch rather than by all of them in turn (PR #1735 review
	// 4306950752). A fetch that fails leaves its id queued; the others' are
	// still taken, since the deck issued them to this receiver.
	const outcomes = await Promise.all(
		ids.map((id) =>
			current.pi
				.exec(deckBin, buildInboxDoorbellFetchArgv(id, rx, INBOX_ADAPTER_INTEGRATION), { signal: ctx.signal, timeout: 5000 })
				.catch(() => undefined),
		),
	);
	const fetched: InboxMessage[] = [];
	for (const outcome of outcomes) {
		const messages = outcome?.code === 0 ? parseInboxFetch(outcome.stdout) : undefined;
		fetched.push(...(messages ?? []).filter((m) => typeof m.a === "number" && typeof m.e === "number"));
	}
	if (fetched.length === 0) {
		return HANDLED;
	}
	proc.receiver.take(fetched as InboxReceiverMessage[]);
	// No await from here to the return.
	const handing = fetched.filter((m) => !alreadyCommitted(proc, m));
	if (handing.length === 0) {
		return HANDLED;
	}
	const refs = handing.map((m) => inboxRef(m as InboxRef));
	const live =
		!proc.disposed &&
		!instance.shutdown &&
		proc.current?.token === token &&
		proc.receiver.epoch() === epoch;
	let ready = false;
	try {
		ready = live && refs.every((ref) => proc.receiver.ready(ref, approval));
	} catch {
		ready = false;
	}
	if (!ready) {
		for (const ref of refs) {
			refuse(proc, ref);
		}
		return HANDLED;
	}
	for (const ref of refs) {
		proc.receiver.calling(ref);
		proc.passed.set(inboxKey(ref), { ref, token, stage: "input", run: proc.runs, queued: false, suspect: false, lost: false });
		watchQueue(proc, inboxKey(ref), ctx);
		noteId(proc, ref);
	}
	proc.receiver.handed(refs);
	return { action: "transform", text: inboxUserText(handing) };
}

/**
 * Whether `instance` witnesses a commit in `proc`: it has not shut down, and
 * it is `proc`'s current session; or its `session_start` waits on a version
 * replacement while no session is current there — the reload's successor,
 * the one session Pi runs then (§0.5). A commit never comes from a change of
 * session.
 */
function witness(proc: InboxProcess, instance: Instance): boolean {
	if (instance.shutdown) {
		return false;
	}
	if (instance.proc === proc) {
		return !proc.disposed && instance.token !== undefined && proc.current?.token === instance.token;
	}
	return instance.proc === undefined && instance.starting && proc.current === undefined && carriesState(proc);
}

/**
 * Pi committed `message` (`message_start`): every attempt of each deck id a
 * user message carries that passed a hand-over point, or that was sent and
 * reached Pi around the gate, is committed, when `instance` is a witness.
 * One made while a replacement drains `proc` is kept by its receiver as an
 * unsent report and carried to the successor, whose watcher publishes it.
 */
function commit(proc: InboxProcess, instance: Instance, message: unknown): void {
	if (!witness(proc, instance)) {
		return;
	}
	const refs: InboxRef[] = [];
	for (const id of inboxIdsToAck(message)) {
		for (const key of proc.byId.get(id) ?? []) {
			if (proc.committed.has(key)) {
				continue;
			}
			const passed = proc.passed.get(key);
			const sent = proc.sent.get(key);
			if (passed !== undefined) {
				refs.push(passed.ref);
			} else if (sent !== undefined) {
				proc.sent.delete(key);
				clearRecheck(proc, key);
				proc.receiver.calling(sent.ref);
				refs.push(sent.ref);
			} else {
				continue;
			}
			proc.passed.delete(key);
			rememberKey(proc.committed, key);
		}
		proc.byId.delete(id);
	}
	if (refs.length > 0) {
		proc.receiver.committed(refs);
	}
}

/** A `before_agent_start` prompt carries these attempts: they are about to run, or be refused. */
function starting(proc: InboxProcess, instance: Instance, prompt: unknown): void {
	if (proc.current?.token !== instance.token) {
		return;
	}
	const ids = new Set(inboxIdsIn(prompt));
	for (const passed of proc.passed.values()) {
		if (passed.token === instance.token && passed.stage === "input" && ids.has(passed.ref.id)) {
			passed.stage = "start";
			passed.suspect = false;
		}
	}
}

/**
 * Pi settled a run, or a prompt it refused (PRD #1666 §2.3). Only positive
 * evidence releases an attempt past the gate as `lost` — possibly handed, so
 * the deck decides on its checked, marked repeat:
 * - one a `before_agent_start` prompt carried, at a settle with no run
 *   active, after a settle while another run was active (Pi's refusal) or
 *   with nothing queued;
 * - one past the gate, at a settle with no run active and nothing queued,
 *   after a run started since it passed (Escape pulled it back, or the run
 *   dropped it), or once it was seen queued behind the run this settle ends
 *   (`watchQueue`: Escape or a dequeue took it out of the queue).
 * One still in preflight, or a Pi that cannot be asked, keeps it held. A sent
 * attempt the gate never saw, at a settle with nothing queued after a run
 * started since it was sent, never reached the gate: positively not handed.
 */
function settled(proc: InboxProcess, instance: Instance, ctx: ExtensionContext): void {
	if (proc.disposed || proc.current?.token !== instance.token) {
		return;
	}
	const running = askRunning(ctx);
	const queued = askQueued(ctx);
	for (const [key, passed] of proc.passed) {
		if (passed.token !== instance.token || passed.lost || proc.committed.has(key)) {
			continue;
		}
		let release = false;
		if (passed.stage === "start") {
			if (running === true) {
				passed.suspect = true;
			} else if (running === false && (passed.suspect || queued === false)) {
				release = true;
			}
		} else if (running === false && queued === false && (passed.run < proc.runs || passed.queued)) {
			release = true;
		}
		if (release) {
			passed.lost = true;
			proc.receiver.lost(passed.ref);
		}
	}
	if (running === false && queued === false) {
		for (const [key, sent] of proc.sent) {
			if (sent.token === instance.token && sent.run < proc.runs) {
				proc.sent.delete(key);
				refuse(proc, sent.ref);
			}
		}
	}
}

/** One extension instance: Pi builds one per session (`/new`, resume, fork, reload). */
interface Instance {
	token: number | undefined;
	/** Its `session_shutdown` ran: no longer current, and no longer a witness. */
	shutdown: boolean;
	/**
	 * The process's receiver it uses, bound at its `session_start`: a delayed
	 * callback of this instance keeps acting on that one, not on a receiver a
	 * later version put in its place — except `session_shutdown`, which acts
	 * on the successor a compatible replacement carried its records to
	 * (`holding`).
	 */
	proc: InboxProcess | undefined;
	/** Its `session_start` is waiting for the process's receiver (a version replacement). */
	starting: boolean;
}

/** A session started in `instance`: it becomes the current one and is handed what waited. */
function sessionStart(proc: InboxProcess, instance: Instance, pi: ExtensionAPI, ctx: ExtensionContext, reason: unknown): void {
	proc.nextToken += 1;
	instance.token = proc.nextToken;
	if (reason === "reload" && proc.reloadedFrom !== undefined) {
		// A reload keeps the same agent and its queue: what the outgoing
		// instance passed is this one's now.
		for (const passed of proc.passed.values()) {
			if (passed.token === proc.reloadedFrom) {
				passed.token = instance.token;
			}
		}
	}
	proc.reloadedFrom = undefined;
	proc.current = { token: instance.token, pi, ctx };
	proc.receiver.ensure();
	pump(proc);
}

/**
 * `instance`'s session ended. A `/new`, resume or fork drops Pi's follow-up
 * queue with the session (PRD #1666 §0.7), so what this instance passed and
 * Pi has not committed is released `lost` — possibly handed, never not handed
 * — and what it sent that the gate has not seen goes back, uncalled, to the
 * front of `pending` for the successor, which submits it again under a new
 * tag (the old one is consumed at any gate). A reload keeps the agent and its
 * queue, so it releases nothing: what passed is the reload's successor's. The
 * watcher is left running either way.
 *
 * A shutdown that arrives after its successor became current does all of
 * this for its own attempts too — a reload's then go straight to the current
 * session — and only leaves `current` alone; `reporting` drops its Idle. One
 * that arrives after a version replacement carried its process's records
 * (`holding`) does it in the successor, which now holds those attempts: a
 * reload's go to the successor's current session, or to the next one to
 * start, and a `/new`'s are released by the successor's receiver, never
 * through the disposed one.
 */
function sessionShutdown(bound: InboxProcess, instance: Instance, reason: unknown): void {
	instance.shutdown = true;
	if (instance.token === undefined) {
		return;
	}
	const proc = holding(bound);
	const wasCurrent = proc.current?.token === instance.token;
	if (reason === "reload") {
		if (wasCurrent) {
			proc.reloadedFrom = instance.token;
		} else if (proc.current === undefined) {
			proc.reloadedFrom ??= instance.token;
		} else {
			for (const passed of proc.passed.values()) {
				if (passed.token === instance.token) {
					passed.token = proc.current.token;
				}
			}
		}
	} else {
		for (const [key, passed] of proc.passed) {
			if (passed.token === instance.token && !passed.lost && !proc.committed.has(key)) {
				passed.lost = true;
				proc.receiver.lost(passed.ref);
			}
		}
		const unseen: InboxMessage[] = [];
		for (const [key, sent] of proc.sent) {
			if (sent.token === instance.token) {
				proc.sent.delete(key);
				clearRecheck(proc, key);
				unseen.push(sent.message);
			}
		}
		proc.pending.unshift(...unseen);
	}
	if (wasCurrent) {
		proc.current = undefined;
	} else {
		pump(proc);
	}
}

/**
 * The process state holding `proc`'s records: `proc` itself, or — once a
 * version replacement disposed it and carried them (`Transfer` hands the same
 * maps on) — the successor that holds them now.
 */
function holding(proc: InboxProcess): InboxProcess {
	if (!proc.disposed) {
		return proc;
	}
	const latest = holder[INBOX_PROCESS] as Partial<InboxProcess> | undefined;
	return latest !== undefined && latest !== proc && latest.passed === proc.passed ? (latest as InboxProcess) : proc;
}

/**
 * Whether `instance` still reports `eventName` (§0.3): only while it is its
 * process's current session, so a stale Thinking never claims a turn for a
 * successor. Its own `session_shutdown` is reported while no session is
 * current — in Pi's order, ahead of its successor's first report — and
 * dropped once a successor is current, here or in a replacement's receiver:
 * a delayed shutdown's Idle would land after the successor's Thinking and
 * end its turn. The successor's own `session_start` reports Idle.
 */
function reporting(proc: InboxProcess, instance: Instance, eventName: string): boolean {
	if (eventName !== "session_shutdown") {
		return instance.token !== undefined && proc.current?.token === instance.token;
	}
	if (proc.current !== undefined && proc.current.token !== instance.token) {
		return false;
	}
	// A token is only unique within one process's state, so a replacement's
	// current session is a successor whatever its number.
	const latest = holder[INBOX_PROCESS] as { current?: unknown } | undefined;
	return latest === undefined || latest === proc || latest.current === undefined;
}

/**
 * Whether a replacement carries `old`'s state: a released receipt version
 * whose `Transfer` shape is this one's.
 */
function carriesState(old: Partial<InboxProcess>): boolean {
	return typeof old.version === "number" && old.version >= FIRST_RECEIPT_VERSION && old.stateVersion === INBOX_STATE_VERSION;
}

/**
 * A fresh process state, carrying `transfer` from a replaced one when there is
 * one. A replacement registers `--resumed`, so the deck asks about the
 * attempts the old receiver held; with nothing transferred, the new one
 * cannot say what the old one handed, so it answers every revoke
 * `already_handed` and resumes `unknown` — possibly handed, never not handed.
 */
function createProcess(transfer?: Transfer, replaced = false, tokenFloor = 0): InboxProcess {
	const proc: InboxProcess = {
		version: INBOX_VERSION,
		stateVersion: INBOX_STATE_VERSION,
		receiver: undefined as unknown as InboxReceiver,
		sent: transfer?.sent ?? new Map(),
		tags: transfer?.tags ?? new Map(),
		passed: transfer?.passed ?? new Map(),
		byId: transfer?.byId ?? new Map(),
		committed: transfer?.committed ?? new Set(),
		pending: transfer?.pending ?? [],
		current: undefined,
		nextToken: Math.max(transfer?.nextToken ?? 0, tokenFloor),
		nextTag: transfer?.nextTag ?? 0,
		tagPrefix: transfer?.tagPrefix ?? (Math.random().toString(36).slice(2, 10) || "0"),
		runs: transfer?.runs ?? 0,
		pumping: false,
		// The outgoing receiver's queue, so its instances' last reports and
		// this one's stay in Pi's order.
		reportQueue: transfer?.reportQueue ?? createSerialQueue(),
		rechecks: new Map(),
		// A reload's successor takes over what the reloaded instance passed.
		reloadedFrom: transfer?.reloadedFrom,
		replacing: undefined,
		disposed: false,
		notInDeck: false,
	};
	proc.receiver = inboxReceiver({
		agent: "pi",
		active: () => !proc.disposed && !proc.notInDeck,
		deliver: (message) => deliver(proc, message as InboxMessage),
		exited: (code) => {
			if (code === WATCHER_NOT_IN_DECK_EXIT) {
				proc.notInDeck = true;
			}
		},
		registered: () => pump(proc),
		state: (transfer?.receiver as InboxReceiverState | undefined) ?? (replaced ? { forgot: true } : undefined),
	});
	// What a sent attempt the old receiver never saw at the gate waits for now.
	for (const key of proc.sent.keys()) {
		armRecheck(proc, key);
	}
	return proc;
}

/**
 * The process's receiver, created on first use. An extension from before the
 * receipt stream left its own watcher and state: its instances are retired
 * and its watcher stopped, since this receiver registers in its place.
 */
function inboxProcess(): InboxProcess {
	const existing = holder[INBOX_PROCESS] as InboxProcess | undefined;
	if (existing !== undefined) {
		return existing;
	}
	const legacy = holder[LEGACY_INBOX] as { generation?: number; watcher?: { stdin?: { end(): unknown } | null; kill(signal?: string): unknown } } | undefined;
	if (legacy !== undefined) {
		legacy.generation = (legacy.generation ?? 0) + 1;
		try {
			legacy.watcher?.stdin?.end();
			legacy.watcher?.kill("SIGTERM");
		} catch {
			// Already gone.
		}
		legacy.watcher = undefined;
	}
	const proc = createProcess();
	holder[INBOX_PROCESS] = proc;
	return proc;
}

/**
 * PRD #1666 §0.5: the process's receiver came from a different version of this
 * file (a reload after the extension was refreshed). It is disposed at once —
 * it hands nothing more to Pi and requests no grant — and its watcher
 * drained, awaited: until the old watcher exits, or until the drain's
 * deadline forces it (a kill is sent and this receiver stops reading it,
 * without seeing the exit), no successor starts. While it drains,
 * the outgoing receiver still reads its watcher (as that version's code
 * does): a message is held and joins `pending`, a revoke is answered in its
 * state, and a commit is kept as an unsent report. Then a new receiver
 * starts, holding the old one's attempts, `pending`, reports, gate records
 * and the reload's ownership of what passed when their shape is this
 * version's: it answers the deck's resumes `retained`, hands `pending` to
 * the successor and publishes the kept reports once it registers. Holding
 * nothing otherwise — a different shape, or a version older than the first
 * released one (`carriesState`) — it answers `unknown`: possibly handed,
 * never not handed. Session tokens keep counting up across either, so no
 * successor reuses an outgoing session's number. One replacement serves
 * every instance that asks while it runs.
 */
async function replaceVersion(): Promise<InboxProcess> {
	for (;;) {
		const old = holder[INBOX_PROCESS] as Partial<InboxProcess> | undefined;
		if (old === undefined) {
			return inboxProcess();
		}
		if (old.version === INBOX_VERSION) {
			return old as InboxProcess;
		}
		if (old.replacing !== undefined) {
			await old.replacing;
			continue;
		}
		old.replacing = (async () => {
			old.disposed = true;
			const receiver = old.receiver as Partial<InboxReceiver> | undefined;
			try {
				if (typeof receiver?.drain === "function") {
					await receiver.drain();
				} else {
					receiver?.stop?.();
				}
			} catch {
				// Gone already.
			}
			let transfer: Transfer | undefined;
			if (carriesState(old) && typeof receiver?.exportState === "function") {
				transfer = {
					receiver: receiver.exportState(),
					sent: old.sent ?? new Map(),
					tags: old.tags ?? new Map(),
					passed: old.passed ?? new Map(),
					byId: old.byId ?? new Map(),
					committed: old.committed ?? new Set(),
					pending: old.pending ?? [],
					nextToken: old.nextToken ?? 0,
					nextTag: old.nextTag ?? 0,
					tagPrefix: old.tagPrefix ?? "0",
					runs: old.runs ?? 0,
					reloadedFrom: old.reloadedFrom,
					reportQueue: old.reportQueue,
				};
				for (const timer of old.rechecks?.values() ?? []) {
					clearTimeout(timer);
				}
			}
			holder[INBOX_PROCESS] = createProcess(transfer, true, typeof old.nextToken === "number" ? old.nextToken : 0);
		})();
		await old.replacing;
	}
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

	// --- PRD #1666: this instance of the extension -------------------------
	const instance: Instance = { token: undefined, shutdown: false, proc: undefined, starting: false };
	/** Keep the current session's context fresh for the receiver's questions to Pi. */
	const seen = (ctx: ExtensionContext): void => {
		const current = instance.proc?.current;
		if (current !== undefined && current.token === instance.token) {
			current.ctx = ctx;
		}
	};

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
	// PRD #1497: the run's last assistant reply, attached to `agent_settled`.
	// Observed synchronously in each handler, so it follows Pi's event order.
	const replies = createTurnReplyTracker();
	// PRD #1666 §0.3: every instance in the process reports through one queue,
	// each report with its retries run to completion before the next, so the
	// deck receives them in the order the handlers queued them, across a
	// session replacement too; `reporting` decides which are sent.
	const report = (eventName: string, event: unknown, ctx: ExtensionContext): Promise<void> => {
		replies.observe(eventName, event);
		const reply = eventName === "agent_settled" ? replies.take() : undefined;
		const proc = instance.proc;
		if (proc === undefined || !reporting(proc, instance, eventName)) {
			return Promise.resolve();
		}
		return proc.reportQueue(async () => {
			const decided = piEventReport(eventName, event, ctx.cwd, reply);
			if (!decided) {
				return;
			}
			reportCwd = ctx.cwd;
			await reporter.send(decided, ctx.signal);
		});
	};

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
			// The receiver first — its watcher already runs unless this is the
			// process's first session — then the announcement that holds the
			// doorbell off until it registers, then the status (Idle — a Pi
			// pane awaiting its first prompt is idle, matching every other
			// backend's session-start).
			wrapDialogs(ctx);
			instance.starting = true;
			let proc: InboxProcess;
			try {
				proc = await replaceVersion();
			} finally {
				instance.starting = false;
			}
			instance.proc = proc;
			sessionStart(proc, instance, pi, ctx, (event as { reason?: unknown } | undefined)?.reason);
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
			seen(ctx);
			try {
				await report("before_agent_start", event, ctx);
			} finally {
				// Once its own hook has run, a hand-over goes on to run or be refused.
				if (instance.proc !== undefined) {
					starting(instance.proc, instance, event.prompt);
				}
			}
		}),
	);
	// PRD #1666 §2.2: the gate. The extension's own hand-over passes here —
	// `transform`, the point it is handed to Pi — or is consumed; a gate that
	// throws consumes it too, never passing tagged text on. A submitted
	// doorbell line is consumed, or transformed into the content it announced.
	// Issue #1567: otherwise, a prompt submitted while Pi is busy is queued and
	// never reaches `before_agent_start`; `piEventReport` reports exactly that
	// `input` and ignores every other. Returns nothing then, so Pi carries on
	// with the input unchanged.
	pi.on("input", async (event, ctx) => {
		const tagged =
			(event as { source?: unknown } | undefined)?.source === "extension" &&
			untagHandOver((event as { text?: unknown }).text) !== undefined;
		let answer: InputAnswer;
		try {
			const proc = instance.proc;
			answer = proc === undefined ? (tagged ? HANDLED : undefined) : gate(proc, instance, event, ctx);
		} catch {
			answer = tagged ? HANDLED : undefined;
		}
		if (answer?.action === "handled") {
			return answer;
		}
		if (answer !== undefined) {
			// R1-01: the gate's transform goes back to Pi with no await after
			// its fence, so nothing the handler waits for can outlive the
			// session or the approval it checked. The hand-over's own report
			// is queued here, in Pi's order, and not waited for.
			try {
				seen(ctx);
				void report("input", { ...event, text: answer.text }, ctx).catch(() => undefined);
			} catch {
				// Reporting never stops a hand-over.
			}
			return answer;
		}
		return guarded(async () => {
			seen(ctx);
			const proc = instance.proc;
			const input = event as { text?: unknown; source?: unknown };
			if (proc !== undefined && input.source !== "extension") {
				const ids = doorbellIdsIn(input.text);
				if (ids.length > 0) {
					return doorbell(proc, instance, ids, ctx);
				}
			}
			await report("input", event, ctx);
			return undefined;
		})(event, ctx);
	});
	// PRD #1666 §2.4: the user message carrying deck messages started — their
	// commit. While this instance's session_start waits on a version
	// replacement, the receiver it waits on (the draining one, then its
	// successor) is the one that keeps it.
	pi.on(
		"message_start",
		guarded((event) => {
			const proc = instance.proc ?? (instance.starting ? (holder[INBOX_PROCESS] as InboxProcess | undefined) : undefined);
			if (proc !== undefined) {
				commit(proc, instance, event.message);
			}
		}),
	);
	pi.on(
		"agent_start",
		guarded(async (event, ctx) => {
			seen(ctx);
			const proc = instance.proc;
			if (proc !== undefined && proc.current?.token === instance.token) {
				proc.runs += 1;
			}
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
			seen(ctx);
			if (instance.proc !== undefined) {
				settled(instance.proc, instance, ctx);
			}
			await report("agent_settled", event, ctx);
		}),
	);
	pi.on(
		"session_shutdown",
		guarded(async (event, ctx) => {
			const proc = instance.proc;
			if (proc !== undefined) {
				sessionShutdown(proc, instance, (event as { reason?: unknown } | undefined)?.reason);
			}
			await report("session_shutdown", event, ctx);
		}),
	);
}
