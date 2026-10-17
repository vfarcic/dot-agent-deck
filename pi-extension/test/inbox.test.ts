/** Tests the actual Pi API glue against Pi 0.84.4's measured event contract. */
import assert from "node:assert/strict";
import childProcess from "node:child_process";
import { EventEmitter } from "node:events";
import { registerHooks, syncBuiltinESMExports } from "node:module";
import { afterEach, beforeEach, mock, test } from "node:test";

// Pi resolves typebox from its own runtime. These tests need its schema
// builders only to register the unrelated tools, not to validate tool inputs.
const hooks = registerHooks({
	resolve(specifier, context, nextResolve) {
		if (specifier === "typebox") {
			return { url: "data:text/javascript,export const Type = new Proxy({}, {get:()=> (...args)=>({args})});", shortCircuit: true };
		}
		return nextResolve(specifier, context);
	},
});
const { default: extension } = await import("../src/index.ts");
const { INBOX_ADMISSION_RECHECK_MS, INBOX_ACK_CONFIRM_ATTEMPTS } = await import("../src/orchestrator.ts");
hooks.deregister();

type Handler = (event: any, ctx: any) => unknown;
const ID = "m-k3xzq2ab-173ce9ac54b742c9";
const BLOCK = `[dot-agent-deck message ${ID} from the orchestrator]\nWrite a sentinel.\n[end of dot-agent-deck message ${ID}]`;
const delivery = (text: string, id = ID) => ({ id, subject: "Pi inbox fixture", from: "the orchestrator", text });

class Watcher extends EventEmitter {
	stdout = Object.assign(new EventEmitter(), { setEncoding: (_encoding: string) => this.stdout });
	stderr = new EventEmitter();
	pid: number;
	writes: string[] = [];
	ended = false;
	killed = false;
	stdin = {
		write: (line: string) => { this.writes.push(String(line)); return true; },
		end: () => { this.ended = true; },
		on: () => this.stdin,
	};
	constructor(pid: number) { super(); this.pid = pid; }
	kill() { this.killed = true; return true; }
}

let spawned: { bin: string; argv: string[]; options: any; child: Watcher }[];
beforeEach(() => {
	spawned = [];
	// What Pi committed and left unconfirmed is kept per process, across
	// session replacements; each test here is its own process's worth.
	const shared = (globalThis as any)[Symbol.for("dot-agent-deck.pi-inbox")];
	shared?.committed?.clear();
	shared?.unconfirmed?.clear();
	mock.method(childProcess, "spawn", (bin: string, argv: string[], options: any) => {
		const child = new Watcher(41000 + spawned.length);
		spawned.push({ bin, argv, options, child });
		return child;
	});
	syncBuiltinESMExports();
});
afterEach(() => { mock.restoreAll(); syncBuiltinESMExports(); });

function fixture() {
	const handlers = new Map<string, Handler>();
	const sent: { text: string; options: any }[] = [];
	const ctx = { cwd: "/scratch/pi-inbox", signal: new AbortController().signal, isIdle: () => true };
	const pi = {
		registerTool() {},
		on(name: string, handler: Handler) { handlers.set(name, handler); },
		exec: async (_bin: string, _argv: string[], _options?: unknown) => ({ code: 0, stdout: "", stderr: "", killed: false }),
		sendUserMessage(text: string, options: any) { sent.push({ text, options }); return undefined; },
	};
	extension(pi as any);
	const emit = async (name: string, event: any = {}) => { await handlers.get(name)?.(event, ctx); };
	const watcher = () => {
		const found = spawned.slice().reverse().find(s => s.argv.includes("inbox") && s.argv.includes("wait"));
		assert.ok(found, "session_start must spawn the Pi inbox stream watcher (get-seed alone cannot wake idle Pi)");
		return found.child;
	};
	const deliver = (child: Watcher, text = BLOCK) => child.stdout.emit("data", Buffer.from(JSON.stringify(delivery(text)) + "\n"));
	return { handlers, sent, ctx, pi, emit, watcher, deliver };
}

/// Scenario: Start a Pi session and feed a framed inbox block in split stdout chunks. The extension hands the intact block to Pi with followUp, without acknowledging before a committed user message.
test("pi_inbox_001_session_start_streams_framed_messages_always_as_follow_up", async () => {
	const f = fixture(); await f.emit("session_start", { reason: "startup" });
	const child = f.watcher();
	const spawn = spawned.find(s => s.child === child)!;
	assert.deepEqual(spawn.argv, ["inbox", "wait", "--agent", "pi", "--stream"]);
	assert.equal(spawn.options.stdio[0], "pipe", "EOF must end an orphaned watcher");
	const line = JSON.stringify(delivery(BLOCK)) + "\n";
	child.stdout.emit("data", Buffer.from(line.slice(0, 19)));
	assert.equal(f.sent.length, 0);
	child.stdout.emit("data", Buffer.from(line.slice(19)));
	assert.equal(f.sent.length, 1);
	assert.ok(f.sent[0].text.includes(BLOCK), "retain the complete framed block, allowing session framing around it");
	assert.deepEqual(f.sent[0].options, { deliverAs: "followUp" });
	assert.deepEqual(child.writes, [], "sendUserMessage returns undefined, not a resolve-ack");
	await f.emit("session_shutdown", { reason: "quit" });
});

/// Scenario: Pi emits input before committing an extension message, then reports assistant and user message starts. Only the user message carrying the deck opener may acknowledge the delivery.
test("pi_inbox_002_ack_waits_for_message_start_user_never_input", async () => {
	const f = fixture();
	assert.ok(f.handlers.has("message_start"), "Pi inbox ack must subscribe to committed message_start, not pre-commit input");
	await f.emit("session_start"); const child = f.watcher(); f.deliver(child);
	await f.emit("input", { text: BLOCK, source: "extension", streamingBehavior: "followUp" });
	await f.emit("before_agent_start", { prompt: BLOCK });
	assert.deepEqual(child.writes, []);
	await f.emit("message_start", { message: { role: "assistant", content: [{ type: "text", text: BLOCK }] } });
	await f.emit("message_start", { message: { role: "user", content: [{ type: "text", text: "ordinary user text" }] } });
	assert.deepEqual(child.writes, []);
	await f.emit("message_start", { message: { role: "user", content: [{ type: "text", text: BLOCK }] } });
	assert.deepEqual(child.writes.map(s => JSON.parse(s)), [{ ack: [ID] }]);
	await f.emit("session_shutdown");
});

/// Scenario: While Pi is busy, feed three quick inbox messages and then commit them one by one at turn boundaries. Every send uses followUp and every acknowledgement follows its own committed message.
test("pi_inbox_003_busy_messages_keep_order_and_ack_on_each_commit", async () => {
	const f = fixture(); f.ctx.isIdle = () => false;
	await f.emit("session_start"); const child = f.watcher();
	const ids = [ID, "m-k3xzq2ab-bbbbbbbb54b742c9", "m-k3xzq2ab-cccccccc54b742c9"];
	const blocks = ids.map(id => BLOCK.replaceAll(ID, id));
	child.stdout.emit("data", Buffer.from(blocks.map((text, i) => JSON.stringify(delivery(text, ids[i]))).join("\n") + "\n"));
	assert.equal(f.sent.length, blocks.length);
	f.sent.forEach((message, i) => {
		assert.ok(message.text.includes(blocks[i]), "retain inbox order and each complete block");
		assert.deepEqual(message.options, { deliverAs: "followUp" });
	});
	assert.deepEqual(child.writes, []);
	for (const text of blocks) await f.emit("message_start", { message: { role: "user", content: [{ type: "text", text }] } });
	assert.deepEqual(child.writes.map(s => JSON.parse(s)), ids.map(id => ({ ack: [id] })));
	await f.emit("session_shutdown");
});

/// Scenario: Replace a Pi session with /new, instantiate the extension again, and send a late callback from the old watcher. The old watcher is closed and killed, and only the current instance delivers into the new session.
test("pi_inbox_004_session_replacement_stops_old_watcher_and_ignores_late_callbacks", async () => {
	const old = fixture(); await old.emit("session_start"); const previous = old.watcher();
	await old.emit("session_shutdown", { reason: "new" });
	assert.equal(previous.ended, true); assert.equal(previous.killed, true);
	const current = fixture(); await current.emit("session_start", { reason: "new" }); const next = current.watcher();
	assert.notEqual(previous.pid, next.pid);
	let staleCalls = 0;
	old.pi.sendUserMessage = () => { staleCalls++; throw new Error("This extension ctx is stale after session replacement"); };
	assert.doesNotThrow(() => old.deliver(previous));
	assert.equal(staleCalls, 0, "a stale generation must do nothing, not merely catch a stale API call");
	assert.deepEqual(old.sent, []);
	current.deliver(next); assert.equal(current.sent.length, 1);
	await current.emit("session_shutdown", { reason: "quit" });
});

/// Scenario: A live Pi callback throws synchronously and malformed watcher input arrives. Neither may escape the event callback or acknowledge a task that Pi did not commit.
test("pi_inbox_005_callback_failures_do_not_crash_pi_or_ack_lost_messages", async () => {
	const f = fixture(); await f.emit("session_start"); const child = f.watcher();
	f.pi.sendUserMessage = () => { throw new Error("This extension ctx is stale after session replacement"); };
	assert.doesNotThrow(() => f.deliver(child));
	assert.doesNotThrow(() => child.stdout.emit("data", Buffer.from("invalid-json\n")));
	assert.deepEqual(child.writes, []);
	const badEvent = Object.defineProperty({}, "message", { get() { throw new Error("stale event"); } });
	await assert.doesNotReject(() => f.emit("message_start", badEvent));
	Object.defineProperty(f.ctx, "cwd", { get() { throw new Error("This extension ctx is stale"); } });
	await assert.doesNotReject(() => f.emit("agent_start"));
	await f.emit("session_shutdown");
});

/// Scenario: A Pi watcher exits unexpectedly while its extension remains current. After backoff a new stream watcher must start, while shutdown must prevent further respawns.
test("pi_inbox_006_unexpected_exit_respawns_with_backoff_but_shutdown_does_not", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const f = fixture(); await f.emit("session_start"); const child = f.watcher();
	child.emit("exit", 1, null);
	assert.equal(spawned.length, 1, "respawn must wait for backoff");
	t.mock.timers.tick(1000);
	assert.equal(spawned.length, 2, "unexpected exit must respawn after the initial one-second backoff");
	const next = f.watcher(); await f.emit("session_shutdown"); next.emit("exit", 0, "SIGTERM");
	t.mock.timers.tick(30000); assert.equal(spawned.length, 2);
});

/// Scenario: A fixed doorbell arrives while Pi's watcher is absent during respawn backoff. The input handler consumes it, fetches without acknowledging, and hands the framed task to sendUserMessage with followUp; only the committed user message acknowledges it through the CLI.
test("pi_inbox_007_unarmed_doorbell_is_intercepted_without_model_fetch_or_early_ack", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const f = fixture();
	const commands: string[][] = [];
	f.pi.exec = async (_bin, argv) => {
		commands.push(argv);
		return { code: 0, stdout: argv[0] === "inbox" && argv[1] === "fetch" ? JSON.stringify({ messages: [delivery(BLOCK)] }) : "", stderr: "", killed: false };
	};
	await f.emit("session_start", { reason: "startup" });
	const active = spawned.find(s => s.argv.includes("inbox") && s.argv.includes("wait"));
	active?.child.emit("exit", 1, null);
	const result = await f.handlers.get("input")!({ text: ` dot-agent-deck: message ${ID} waiting, run: dot-agent-deck inbox fetch`, source: "interactive" }, f.ctx) as { action?: string } | undefined;
	assert.equal(result?.action, "handled", "Pi input offers interception: consume the doorbell instead of asking the model to fetch");
	assert.ok(commands.some(argv => argv[0] === "inbox" && argv[1] === "fetch" && argv.includes("--json") && argv.includes("--doorbell") && argv.includes(ID)), "fetch the queued doorbell id without ack, like the OpenCode interceptor");
	assert.equal(f.sent.length, 1); assert.ok(f.sent[0].text.includes(BLOCK));
	assert.deepEqual(f.sent[0].options, { deliverAs: "followUp" });
	assert.ok(!commands.some(argv => argv[0] === "ack"), "input is pre-commit, so it cannot acknowledge");
	await f.emit("message_start", { message: { role: "user", content: [{ type: "text", text: BLOCK }] } });
	assert.ok(commands.some(argv => argv[0] === "ack" && argv.includes(ID)), "without a watcher, committed delivery needs the explicit CLI ack");
	await f.emit("session_shutdown", { reason: "quit" });
});

/// Scenario: With DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS set, a Pi watcher that exits stays down for exactly that long every time, instead of the doubling one-second backoff; a value that is not a whole number of milliseconds leaves the default backoff.
test("pi_inbox_008_watcher_backoff_env_fixes_every_respawn_delay", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const saved = process.env.DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS;
	try {
		process.env.DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS = "45000";
		const f = fixture(); await f.emit("session_start");
		for (let round = 1; round <= 3; round++) {
			f.watcher().emit("exit", 1, null);
			t.mock.timers.tick(44999);
			assert.equal(spawned.length, round, `round ${round}: the watcher must stay down for the configured delay`);
			t.mock.timers.tick(1);
			assert.equal(spawned.length, round + 1, `round ${round}: the watcher must come back once the configured delay has passed`);
		}
		await f.emit("session_shutdown");

		process.env.DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS = "soon";
		spawned = [];
		const g = fixture(); await g.emit("session_start");
		g.watcher().emit("exit", 1, null);
		t.mock.timers.tick(1000);
		assert.equal(spawned.length, 2, "an unreadable value must leave the one-second first backoff");
		await g.emit("session_shutdown");
	} finally {
		if (saved === undefined) delete process.env.DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS;
		else process.env.DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS = saved;
	}
});

/**
 * Pi 0.84.4's prompt admission, as `AgentSession.prompt` runs it for an
 * extension's `sendUserMessage`: `sendUserMessage` returns before any of it, so
 * the caller never sees what happens next. The `input` hooks run first; a hook
 * can consume the prompt (`handled`) or rewrite it (`transform`). A send that
 * then finds a run going waits as a follow-up; one that finds none runs its
 * remaining preflight (auth, compaction, the `before_agent_start` hooks) and
 * hands the prompt to the agent. If another prompt started meanwhile, the agent
 * refuses it ("Agent is already processing a prompt") and Pi emits
 * `agent_settled` for the refused send while that other run is still going, so
 * the context's `signal` is still set; a run that ends settles with none.
 *
 * `holdBeforeInput` and `holdAfterInput` stand for a slow stage on either side
 * of this extension's `input` hook: another extension's hook ordered before
 * it, or auth and compaction after it. `holdAfterStart` stands for another
 * extension's `before_agent_start` hook ordered after this one's. `refuse` fails a send before `input`
 * (compaction already running), and `dropAfterInput` loses one after it with
 * no run (auth failing, or another extension consuming it).
 */
function admission(f: ReturnType<typeof fixture>) {
	let running = false;
	const run = new AbortController();
	const followUps: string[] = [];
	const committed: string[] = [];
	const rejected: string[] = [];
	const dropped: string[] = [];
	let refuseNext = 0;
	let dropNext = 0;
	const tick = () => new Promise<void>(resolve => setImmediate(resolve));
	// Pi awaits the `input` and `before_agent_start` hooks, and this extension's
	// hooks await their card report; a report the CLI is spawned for (a turn's
	// reply on stdin) must therefore finish, as the real CLI does.
	const spawnChild = childProcess.spawn;
	mock.method(childProcess, "spawn", (bin: string, argv: string[], options: any) => {
		const child = (spawnChild as any)(bin, argv, options) as Watcher;
		if (!(argv.includes("inbox") && argv.includes("wait"))) setImmediate(() => child.emit("close", 0, null));
		return child;
	});
	syncBuiltinESMExports();
	// The events Pi does not wait on are fired without awaiting the handler.
	const fire = (name: string, event: any = {}) => { void f.handlers.get(name)?.(event, f.ctx); };
	const commit = async (text: string) => {
		committed.push(text);
		fire("message_start", { message: { role: "user", content: [{ type: "text", text }] } });
		await tick();
	};
	const start = async (text: string) => { running = true; fire("agent_start"); await commit(text); };
	/** A slow stage: each send waits at it until the test lets it through, first come first served. */
	const gate = () => {
		let open = true;
		const waiting: (() => void)[] = [];
		return {
			pass: () => (open ? Promise.resolve() : new Promise<void>(resolve => waiting.push(resolve))),
			close: () => { open = false; },
			releaseOne: () => { waiting.shift()?.(); },
			releaseAll: () => { open = true; while (waiting.length) waiting.shift()!(); },
			get waiting() { return waiting.length; },
		};
	};
	const beforeInput = gate();
	const afterInput = gate();
	const afterStart = gate();
	f.ctx.isIdle = () => !running;
	(f.ctx as any).hasPendingMessages = () => followUps.length > 0;
	Object.defineProperty(f.ctx, "signal", { configurable: true, get: () => (running ? run.signal : undefined) });
	f.pi.sendUserMessage = (text: string, options: any) => {
		f.sent.push({ text, options });
		void (async () => {
			await tick();
			if (refuseNext > 0) { refuseNext--; rejected.push(text); return; }
			await beforeInput.pass();
			const hook = (await f.handlers.get("input")?.({ text, source: "extension", streamingBehavior: running ? options?.deliverAs : undefined }, f.ctx)) as { action?: string; text?: string } | undefined;
			if (hook?.action === "handled") { dropped.push(text); return; }
			const prompt = hook?.action === "transform" ? hook.text! : text;
			if (dropNext > 0) { dropNext--; rejected.push(prompt); return; }
			if (running) { followUps.push(prompt); return; }
			await tick();
			await afterInput.pass();
			await f.handlers.get("before_agent_start")?.({ prompt }, f.ctx);
			await afterStart.pass();
			if (running) { rejected.push(prompt); fire("agent_settled"); await tick(); return; }
			await start(prompt);
		})();
		return undefined;
	};
	const settle = async () => {
		while (followUps.length) await commit(followUps.shift()!);
		running = false;
		fire("agent_settled");
		await tick();
	};
	const quiesce = async () => { for (let i = 0; i < 8; i++) await tick(); };
	return {
		committed, rejected, dropped, tick, settle, quiesce,
		foreign: (text: string) => start(text),
		shutdown: () => fire("session_shutdown"),
		refuse: (n: number) => { refuseNext = n; },
		dropAfterInput: (n: number) => { dropNext = n; },
		holdBeforeInput: () => { beforeInput.close(); return beforeInput; },
		holdAfterInput: () => { afterInput.close(); return afterInput; },
		holdAfterStart: () => { afterStart.close(); return afterStart; },
		get running() { return running; },
	};
}

const idsN = (n: number) => Array.from({ length: n }, (_, i) => `m-k3xzq2ab-${String(i).repeat(8)}54b742c9`);
const line = (id: string) => JSON.stringify(delivery(BLOCK.replaceAll(ID, id), id));
const acksOf = (child: Watcher) => child.writes.flatMap(s => (JSON.parse(s).ack ?? []) as string[]);
const holdsOf = (child: Watcher) => child.writes.flatMap(s => (JSON.parse(s).hold ?? []) as string[]);
const commitsOf = (committed: string[], id: string) => committed.filter(text => text.includes(`[dot-agent-deck message ${id} `)).length;

/// Scenario: Five inbox messages reach an idle Pi in one burst, the way a loaded deck delivers its seed messages. Pi refuses a second prompt that arrives before the first one starts, and reports that refusal itself; the extension must still get every message committed exactly once, in order, and acknowledge each one only after its commit.
test("pi_inbox_009_idle_burst_is_admitted_without_loss_or_duplicates", async () => {
	const f = fixture(); const pi = admission(f);
	await f.emit("session_start"); const child = f.watcher();
	const ids = idsN(5);
	child.stdout.emit("data", Buffer.from(ids.map(line).join("\n") + "\n"));
	for (let round = 0; round < 10 && acksOf(child).length < ids.length; round++) {
		await pi.quiesce();
		if (pi.running) await pi.settle();
	}
	await pi.quiesce();
	assert.deepEqual(pi.rejected, [], "the extension must not race its own sends into Pi's idle admission");
	for (const id of ids) assert.equal(commitsOf(pi.committed, id), 1, `${id} must be committed exactly once`);
	assert.deepEqual(acksOf(child), ids, "each message is acknowledged once, after its commit, in inbox order");
	pi.shutdown();
});

/// Scenario: An inbox message reaches an idle Pi just as another prompt wins Pi's admission, so Pi refuses the deck's message without telling the extension. Once that other run settles with nothing queued, the message is handed over again and committed once; a message handed over while Pi is busy waits as a follow-up and is never handed over twice.
test("pi_inbox_010_send_refused_by_a_competing_prompt_is_handed_over_again", async () => {
	const f = fixture(); const pi = admission(f);
	await f.emit("session_start"); const child = f.watcher();
	const [first, second] = idsN(2);
	child.stdout.emit("data", Buffer.from(line(first) + "\n"));
	await pi.tick();
	await pi.foreign("a prompt the user typed");
	await pi.quiesce();
	assert.equal(pi.rejected.length, 1, "the fixture must reproduce Pi refusing the deck's send");
	assert.deepEqual(acksOf(child), [], "a refused send is never acknowledged");
	await pi.settle();
	await pi.quiesce();
	assert.equal(commitsOf(pi.committed, first), 1, "the refused message is handed over again once the competing run settles");
	child.stdout.emit("data", Buffer.from(line(second) + "\n"));
	await pi.quiesce();
	await pi.settle();
	await pi.quiesce();
	assert.equal(commitsOf(pi.committed, first), 1, "never handed over twice");
	assert.equal(commitsOf(pi.committed, second), 1, "a follow-up queued while busy is committed once and not resent");
	assert.deepEqual(acksOf(child), [first, second]);
	pi.shutdown();
});

/// Scenario: Pi refuses an inbox message while idle and no run follows, the way a send fails when Pi cannot start a turn. While Pi stays idle with nothing queued, the extension hands the message over again after the admission recheck interval, and not before.
test("pi_inbox_011_refused_send_with_no_run_is_retried_after_the_recheck_interval", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const f = fixture(); const pi = admission(f);
	await f.emit("session_start"); const child = f.watcher();
	const [id] = idsN(1);
	pi.refuse(1);
	child.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	assert.equal(pi.rejected.length, 1);
	t.mock.timers.tick(INBOX_ADMISSION_RECHECK_MS - 1);
	await pi.quiesce();
	assert.equal(f.sent.length, 1, "not handed over again before the recheck interval");
	t.mock.timers.tick(1);
	await pi.quiesce();
	assert.equal(f.sent.length, 2, "handed over again once Pi stayed idle with nothing queued");
	assert.equal(commitsOf(pi.committed, id), 1);
	assert.deepEqual(acksOf(child), [id]);
	pi.shutdown();
});

/// Scenario: Another extension's input hook holds an inbox message for longer than the admission recheck while Pi sits idle, so the extension hands it over again; the first hand-over then gets through, its turn ends, and the second follows. Pi must run the message once: the hand-over the retry replaced is consumed at the extension's input hook.
test("pi_inbox_012_a_hand_over_slow_before_input_is_replaced_not_run_twice", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const f = fixture(); const pi = admission(f);
	await f.emit("session_start"); const child = f.watcher();
	const [id] = idsN(1);
	const slow = pi.holdBeforeInput();
	child.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	t.mock.timers.tick(INBOX_ADMISSION_RECHECK_MS);
	await pi.quiesce();
	assert.equal(f.sent.length, 2, "a hand-over that never reached input is handed over again at the recheck");
	slow.releaseOne();
	await pi.quiesce();
	if (pi.running) await pi.settle();
	await pi.quiesce();
	slow.releaseAll();
	await pi.quiesce();
	if (pi.running) await pi.settle();
	await pi.quiesce();
	assert.equal(commitsOf(pi.committed, id), 1, "Pi runs the message once");
	assert.equal(pi.dropped.length, 1, "the replaced hand-over is consumed at input");
	assert.deepEqual(acksOf(child), [id]);
	pi.shutdown();
});

/// Scenario: An inbox message passes the extension's input hook and then waits in Pi's own preflight (auth, compaction) for longer than the admission recheck while Pi looks idle; its turn then runs and ends. The extension must not hand the message over again on a timer, so Pi runs it once.
test("pi_inbox_013_a_hand_over_slow_after_input_is_never_retried_on_a_timer", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const f = fixture(); const pi = admission(f);
	await f.emit("session_start"); const child = f.watcher();
	const [id] = idsN(1);
	const slow = pi.holdAfterInput();
	child.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	t.mock.timers.tick(INBOX_ADMISSION_RECHECK_MS * 3);
	await pi.quiesce();
	assert.equal(f.sent.length, 1, "a hand-over Pi's input hook has seen is not handed over again on a timer");
	slow.releaseOne();
	await pi.quiesce();
	if (pi.running) await pi.settle();
	await pi.quiesce();
	slow.releaseAll();
	await pi.quiesce();
	if (pi.running) await pi.settle();
	await pi.quiesce();
	assert.equal(commitsOf(pi.committed, id), 1, "Pi runs the message once");
	assert.deepEqual(acksOf(child), [id]);
	pi.shutdown();
});

/// Scenario: An inbox message passes the extension's input hook and waits in Pi's preflight while a prompt the user typed runs and finishes. That run settling is not a refusal of the waiting message: it is not handed over again, and Pi runs it once when its preflight ends.
test("pi_inbox_014_a_run_settling_during_preflight_is_not_taken_as_a_refusal", async () => {
	const f = fixture(); const pi = admission(f);
	await f.emit("session_start"); const child = f.watcher();
	const [id] = idsN(1);
	const slow = pi.holdAfterInput();
	child.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	await pi.foreign("a prompt the user typed");
	await pi.quiesce();
	await pi.settle();
	await pi.quiesce();
	slow.releaseOne();
	await pi.quiesce();
	if (pi.running) await pi.settle();
	await pi.quiesce();
	slow.releaseAll();
	await pi.quiesce();
	if (pi.running) await pi.settle();
	await pi.quiesce();
	assert.equal(f.sent.length, 1, "nothing refused it, so it is handed over once");
	assert.equal(commitsOf(pi.committed, id), 1, "Pi runs the message once");
	assert.deepEqual(acksOf(child), [id]);
	pi.shutdown();
});

/// Scenario: An inbox message passes the extension's input hook while Pi is idle and is then lost with no run (auth failing, or another extension consuming it). Nothing hands it over again on a timer; when the deck hands the unacknowledged message over again, the extension lets it through and Pi runs it once.
test("pi_inbox_015_a_hand_over_lost_after_input_waits_for_the_decks_redelivery", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const f = fixture(); const pi = admission(f);
	await f.emit("session_start"); const child = f.watcher();
	const [id] = idsN(1);
	pi.dropAfterInput(1);
	child.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	assert.equal(pi.rejected.length, 1, "the fixture must lose the hand-over after input");
	t.mock.timers.tick(INBOX_ADMISSION_RECHECK_MS * 3);
	await pi.quiesce();
	assert.equal(f.sent.length, 1, "no timer retry for a hand-over Pi's input hook has seen");
	child.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	assert.equal(f.sent.length, 2, "the deck's redelivery of the unacknowledged message is let through");
	if (pi.running) await pi.settle();
	await pi.quiesce();
	assert.equal(commitsOf(pi.committed, id), 1);
	assert.deepEqual(acksOf(child), [id]);
	pi.shutdown();
});

/// Scenario: While Pi is busy an inbox message waits in Pi's follow-up queue, and the deck hands the still-unacknowledged message over again. The extension keeps the queued hand-over and ignores the redelivery, so Pi runs the message once when the turn ends.
test("pi_inbox_016_a_redelivery_while_the_hand_over_is_queued_in_pi_is_ignored", async () => {
	const f = fixture(); const pi = admission(f);
	await f.emit("session_start"); const child = f.watcher();
	const [id] = idsN(1);
	await pi.foreign("a prompt the user typed");
	child.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	child.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	assert.equal(f.sent.length, 1, "a hand-over waiting in Pi's queue is not handed over again");
	await pi.settle();
	await pi.quiesce();
	assert.equal(commitsOf(pi.committed, id), 1);
	assert.deepEqual(acksOf(child), [id]);
	pi.shutdown();
});

/// Scenario: A hand-over whose deck message Pi already committed reaches the extension's input hook again. The hook consumes it, so the model never sees the message twice.
test("pi_inbox_017_a_hand_over_of_a_committed_message_is_consumed_at_input", async () => {
	const f = fixture();
	await f.emit("session_start"); const child = f.watcher();
	f.deliver(child);
	assert.equal(f.sent.length, 1);
	const handed = f.sent[0].text;
	const first = await f.handlers.get("input")!({ text: handed, source: "extension" }, f.ctx) as { action?: string; text?: string } | undefined;
	const prompt = first?.action === "transform" ? first.text! : handed;
	assert.ok(prompt.includes(BLOCK));
	await f.emit("message_start", { message: { role: "user", content: [{ type: "text", text: prompt }] } });
	assert.deepEqual(acksOf(child), [ID]);
	const again = await f.handlers.get("input")!({ text: handed, source: "extension" }, f.ctx) as { action?: string } | undefined;
	assert.equal(again?.action, "handled", "a hand-over of a committed message must not reach the model");
	await f.emit("session_shutdown");
});

/// Scenario: Pi refuses an inbox message before its input hook while the extension cannot ask Pi whether it is idle or has anything queued. An unanswerable Pi is never taken as idle with nothing queued, so the admission recheck does not hand the message over again.
test("pi_inbox_018_an_unanswerable_pi_never_triggers_the_recheck_retry", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	for (const broken of ["isIdle", "hasPendingMessages"] as const) {
		const f = fixture(); const pi = admission(f);
		await f.emit("session_start"); const child = f.watcher();
		const [id] = idsN(1);
		pi.refuse(1);
		child.stdout.emit("data", Buffer.from(line(id) + "\n"));
		await pi.quiesce();
		(f.ctx as any)[broken] = () => { throw new Error("This extension ctx is stale"); };
		t.mock.timers.tick(INBOX_ADMISSION_RECHECK_MS * 3);
		await pi.quiesce();
		assert.equal(f.sent.length, 1, `${broken} throwing must not count as an answer`);
		pi.shutdown();
	}
});

/// Scenario: An inbox message passes this extension's before_agent_start hook and then waits in another extension's slow before_agent_start hook while a prompt the user typed runs and finishes. A run ending is not a refusal, so the message is not handed over again, and Pi runs it once.
test("pi_inbox_019_a_run_ending_after_before_agent_start_is_not_taken_as_a_refusal", async () => {
	const f = fixture(); const pi = admission(f);
	await f.emit("session_start"); const child = f.watcher();
	const [id] = idsN(1);
	const slow = pi.holdAfterStart();
	child.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	await pi.foreign("a prompt the user typed");
	await pi.quiesce();
	await pi.settle();
	await pi.quiesce();
	slow.releaseAll();
	await pi.quiesce();
	if (pi.running) await pi.settle();
	await pi.quiesce();
	assert.equal(f.sent.length, 1, "nothing refused it, so it is handed over once");
	assert.equal(commitsOf(pi.committed, id), 1, "Pi runs the message once");
	assert.deepEqual(acksOf(child), [id]);
	pi.shutdown();
});

/// Scenario: Pi runs the extension's input hook before sendUserMessage returns, as it does when this extension's hook is the first one Pi calls. The hand-over is already on record, so the hook lets it through without the line naming it rather than consuming it.
test("pi_inbox_020_an_input_hook_run_inside_send_user_message_lets_the_hand_over_through", async () => {
	const f = fixture();
	const results: Promise<unknown>[] = [];
	f.pi.sendUserMessage = (text: string, options: any) => {
		f.sent.push({ text, options });
		results.push(Promise.resolve(f.handlers.get("input")!({ text, source: "extension" }, f.ctx)));
		return undefined;
	};
	await f.emit("session_start"); const child = f.watcher();
	f.deliver(child);
	const result = await results[0] as { action?: string; text?: string } | undefined;
	assert.equal(result?.action, "transform", "the hand-over goes on, not consumed");
	assert.ok(result?.text?.includes(BLOCK) && !result.text.includes("hand-over"), "without the line naming it");
	await f.emit("session_shutdown");
});

/// Scenario: While Pi is busy with a long turn an inbox message waits in Pi's follow-up queue, and the deck hands the still-unacknowledged message over again, twice. The extension does not hand it over again and answers each redelivery with hold, so the deck does not count it towards its hand-over limit; Pi runs the message once when the turn ends and it is acknowledged then.
test("pi_inbox_021_a_redelivery_queued_behind_a_long_turn_answers_hold", async () => {
	const f = fixture(); const pi = admission(f);
	await f.emit("session_start"); const child = f.watcher();
	const [id] = idsN(1);
	await pi.foreign("a long turn the user started");
	child.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	assert.deepEqual(holdsOf(child), [], "a first hand-over is not a hold");
	for (let n = 1; n <= 2; n++) {
		child.stdout.emit("data", Buffer.from(line(id) + "\n"));
		await pi.quiesce();
		assert.deepEqual(holdsOf(child), Array(n).fill(id), `redelivery ${n} answers hold`);
	}
	assert.equal(f.sent.length, 1, "a hand-over waiting in Pi's queue is not handed over again");
	await pi.settle();
	await pi.quiesce();
	assert.equal(commitsOf(pi.committed, id), 1);
	assert.deepEqual(acksOf(child), [id]);
	pi.shutdown();
});

/// Scenario: While Pi is busy with a long turn, a job the orchestrator offered this worker waits in Pi's follow-up queue, and the deck hands the unclaimed offer over again, twice. The extension answers each redelivery with hold, as it does for a direct message, so the deck keeps the job with this worker instead of offering it to another; Pi runs the job once when the turn ends, and its acknowledgement then claims it.
test("pi_inbox_024_a_redelivered_offer_queued_behind_a_long_turn_answers_hold", async () => {
	const f = fixture(); const pi = admission(f);
	await f.emit("session_start"); const child = f.watcher();
	const [id] = idsN(1);
	const offer = JSON.stringify({ id, subject: "job offered from pool review", from: "the orchestrator", text: BLOCK.replaceAll(ID, id) });
	await pi.foreign("a long turn the user started");
	child.stdout.emit("data", Buffer.from(offer + "\n"));
	await pi.quiesce();
	assert.deepEqual(holdsOf(child), [], "a first hand-over is not a hold");
	for (let n = 1; n <= 2; n++) {
		child.stdout.emit("data", Buffer.from(offer + "\n"));
		await pi.quiesce();
		assert.deepEqual(holdsOf(child), Array(n).fill(id), `redelivery ${n} of the offer answers hold`);
	}
	assert.equal(f.sent.length, 1, "an offer waiting in Pi's queue is not handed over again");
	await pi.settle();
	await pi.quiesce();
	assert.equal(commitsOf(pi.committed, id), 1, "the job runs once");
	assert.deepEqual(acksOf(child), [id], "and its acknowledgement claims it");
	pi.shutdown();
});

/// Scenario: Two inbox messages reach an idle Pi together; the second waits in the extension until the first one's hand-over settles, and the deck hands the second over again meanwhile. The extension answers that redelivery with hold and hands the second message to Pi once, after the first.
test("pi_inbox_022_a_redelivery_held_behind_an_admission_answers_hold", async () => {
	const f = fixture(); const pi = admission(f);
	await f.emit("session_start"); const child = f.watcher();
	const [first, second] = idsN(2);
	const gate = pi.holdAfterInput();
	child.stdout.emit("data", Buffer.from(line(first) + "\n" + line(second) + "\n"));
	await pi.quiesce();
	assert.equal(f.sent.length, 1, "the second waits for the first hand-over to settle");
	child.stdout.emit("data", Buffer.from(line(second) + "\n"));
	await pi.quiesce();
	assert.deepEqual(holdsOf(child), [second]);
	gate.releaseAll();
	for (let round = 0; round < 10 && acksOf(child).length < 2; round++) {
		await pi.quiesce();
		if (pi.running) await pi.settle();
	}
	assert.equal(commitsOf(pi.committed, first), 1);
	assert.equal(commitsOf(pi.committed, second), 1);
	assert.deepEqual(acksOf(child), [first, second]);
	pi.shutdown();
});

/// Scenario: Pi loses a hand-over after its input hook with nothing queued, and the deck hands the message over again. That redelivery replaces the lost hand-over, as before, and is not answered with hold: the deck's count of hand-overs stands, because the first one did not reach the agent.
test("pi_inbox_023_a_redelivery_that_replaces_a_lost_hand_over_is_not_held", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const f = fixture(); const pi = admission(f);
	await f.emit("session_start"); const child = f.watcher();
	const [id] = idsN(1);
	pi.dropAfterInput(1);
	child.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	child.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	assert.equal(f.sent.length, 2, "the redelivery replaces the lost hand-over");
	assert.deepEqual(holdsOf(child), [], "a replacing redelivery is not a hold");
	if (pi.running) await pi.settle();
	await pi.quiesce();
	assert.equal(commitsOf(pi.committed, id), 1);
	assert.deepEqual(acksOf(child), [id]);
	pi.shutdown();
});

/// Scenario: Pi commits an inbox message and the extension writes its acknowledgement to the watcher, which exits before the deck records it. The deck hands the still-unacknowledged message over again on the respawned watcher; the extension does not hand it to Pi a second time and acknowledges it again instead.
test("pi_inbox_025_a_redelivery_of_a_committed_message_is_acknowledged_not_run_again", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const f = fixture(); const pi = admission(f);
	await f.emit("session_start"); const first = f.watcher();
	const [id] = idsN(1);
	first.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	if (pi.running) await pi.settle();
	await pi.quiesce();
	assert.equal(commitsOf(pi.committed, id), 1);
	assert.deepEqual(acksOf(first), [id], "the acknowledgement goes to the watcher");
	first.emit("exit", 1, null);
	t.mock.timers.tick(1000);
	const second = f.watcher();
	assert.notEqual(second, first, "the watcher is respawned");
	second.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	if (pi.running) await pi.settle();
	await pi.quiesce();
	assert.equal(f.sent.length, 1, "a committed message is not handed to Pi again");
	assert.equal(commitsOf(pi.committed, id), 1, "Pi runs it once");
	assert.deepEqual(acksOf(second), [id], "its acknowledgement is sent again");
	pi.shutdown();
});

/// Scenario: Pi commits an inbox message after its watcher exited, so the extension acknowledges it with the deck CLI, which fails. When the deck hands the message over again on the respawned watcher, the extension acknowledges it there and does not hand it to Pi a second time.
test("pi_inbox_026_a_failed_cli_acknowledgement_is_sent_again_on_redelivery", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const f = fixture(); const pi = admission(f);
	const acksByCli: string[] = [];
	f.pi.exec = async (_bin: string, argv: string[]) => {
		if (argv[0] === "ack") acksByCli.push(argv[argv.length - 1]);
		return { code: 1, stdout: "", stderr: "the deck did not answer", killed: false };
	};
	await f.emit("session_start"); const first = f.watcher();
	const [id] = idsN(1);
	const gate = pi.holdAfterInput();
	first.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	first.emit("exit", 1, null);
	gate.releaseAll();
	await pi.quiesce();
	if (pi.running) await pi.settle();
	await pi.quiesce();
	assert.equal(commitsOf(pi.committed, id), 1);
	assert.deepEqual(acksByCli, [id], "with no watcher the CLI acknowledges, and fails");
	t.mock.timers.tick(1000);
	const second = f.watcher();
	assert.notEqual(second, first, "the watcher is respawned");
	second.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	if (pi.running) await pi.settle();
	await pi.quiesce();
	assert.equal(f.sent.length, 1, "a committed message is not handed to Pi again");
	assert.equal(commitsOf(pi.committed, id), 1, "Pi runs it once");
	assert.deepEqual(acksOf(second), [id], "its acknowledgement is sent again on the new watcher");
	pi.shutdown();
});

const ackReply = (reply: object, code = 0) => ({ code, stdout: JSON.stringify(reply), stderr: "", killed: false });

/// Scenario: Pi commits an inbox message and acknowledges it over its watcher, which then exits without forwarding the acknowledgement, and the deck never hands the message over again. The extension confirms the acknowledgement with the deck CLI on its own, retrying while the deck does not answer, and stops once the deck records it.
test("pi_inbox_027_an_unconfirmed_acknowledgement_is_retried_without_another_delivery", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const f = fixture(); const pi = admission(f);
	const acksByCli: string[] = [];
	let unreachable = 2;
	f.pi.exec = async (_bin: string, argv: string[]) => {
		if (argv[0] !== "ack") return { code: 0, stdout: "", stderr: "", killed: false };
		acksByCli.push(argv[argv.length - 1]);
		if (unreachable > 0) {
			unreachable--;
			return ackReply({ acknowledged: false, final: false, reason: "the deck is not reachable" });
		}
		return ackReply({ acknowledged: true });
	};
	await f.emit("session_start"); const first = f.watcher();
	const [id] = idsN(1);
	first.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	if (pi.running) await pi.settle();
	await pi.quiesce();
	assert.equal(commitsOf(pi.committed, id), 1);
	assert.deepEqual(acksOf(first), [id], "the acknowledgement goes to the watcher first");
	first.emit("exit", 1, null);
	for (let i = 0; i < 20; i++) { t.mock.timers.tick(60_000); await pi.quiesce(); }
	assert.deepEqual(acksByCli, [id, id, id], "confirmed with the CLI until the deck records it, then no more");
	assert.equal(f.sent.length, 1, "Pi is handed the message once");
	pi.shutdown();
});

/// Scenario: Pi commits two inbox messages whose acknowledgements never reach the deck: for one the deck keeps not answering, and the other it reports as no longer deliverable. The extension stops retrying the first after a bounded number of attempts and the second at once, and tells the user in Pi which messages the deck could not record as delivered.
test("pi_inbox_028_an_acknowledgement_that_finally_fails_is_reported", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const f = fixture(); const pi = admission(f);
	const notices: string[] = [];
	(f.ctx as any).ui = { notify: (message: string, type?: string) => { notices.push(`${type}: ${message}`); } };
	const [silent, refused] = idsN(2);
	const attempts = new Map<string, number>();
	f.pi.exec = async (_bin: string, argv: string[]) => {
		if (argv[0] !== "ack") return { code: 0, stdout: "", stderr: "", killed: false };
		const id = argv[argv.length - 1];
		attempts.set(id, (attempts.get(id) ?? 0) + 1);
		return id === refused
			? ackReply({ acknowledged: false, final: true, reason: "undeliverable" }, 1)
			: ackReply({ acknowledged: false, final: false, reason: "the deck did not answer" });
	};
	await f.emit("session_start"); const watcher = f.watcher();
	watcher.stdout.emit("data", Buffer.from(`${line(silent)}\n${line(refused)}\n`));
	for (let i = 0; i < 4; i++) {
		await pi.quiesce();
		if (pi.running) await pi.settle();
	}
	assert.equal(commitsOf(pi.committed, silent), 1);
	assert.equal(commitsOf(pi.committed, refused), 1);
	for (let i = 0; i < 40; i++) { t.mock.timers.tick(60_000); await pi.quiesce(); }
	assert.equal(attempts.get(refused), 1, "a refusal by id is final");
	assert.equal(attempts.get(silent), INBOX_ACK_CONFIRM_ATTEMPTS, "a deck that never answers is asked a bounded number of times");
	assert.ok(notices.some(n => n.startsWith("warning: ") && n.includes(silent)), `the user is told about ${silent}: ${notices}`);
	assert.ok(notices.some(n => n.startsWith("warning: ") && n.includes(refused)), `the user is told about ${refused}: ${notices}`);
	const before = [...attempts.values()].reduce((a, b) => a + b, 0);
	for (let i = 0; i < 10; i++) { t.mock.timers.tick(60_000); await pi.quiesce(); }
	assert.equal([...attempts.values()].reduce((a, b) => a + b, 0), before, "nothing is retried after the outcome is reported");
	pi.shutdown();
});

/// Scenario: Pi commits an inbox message whose acknowledgement the deck has not confirmed yet, and the session is then replaced with /new before a confirmation succeeds. The next session's extension instance goes on confirming that acknowledgement with the deck CLI, and when the deck hands the message over again in the new session it is acknowledged, not handed to Pi a second time.
test("pi_inbox_029_an_unconfirmed_acknowledgement_survives_a_session_replacement", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const old = fixture(); const pi = admission(old);
	const [id] = idsN(1);
	old.pi.exec = async (_bin: string, argv: string[]) =>
		argv[0] === "ack"
			? ackReply({ acknowledged: false, final: false, reason: "the deck is not reachable" })
			: { code: 0, stdout: "", stderr: "", killed: false };
	await old.emit("session_start"); const first = old.watcher();
	first.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	if (pi.running) await pi.settle();
	await pi.quiesce();
	assert.equal(commitsOf(pi.committed, id), 1);
	first.emit("exit", 1, null);
	t.mock.timers.tick(1000); await pi.quiesce();
	await old.emit("session_shutdown", { reason: "new" });

	const current = fixture();
	const acksByCli: string[] = [];
	current.pi.exec = async (_bin: string, argv: string[]) => {
		if (argv[0] !== "ack") return { code: 0, stdout: "", stderr: "", killed: false };
		acksByCli.push(argv[argv.length - 1]);
		return ackReply({ acknowledged: true });
	};
	await current.emit("session_start", { reason: "new" }); const next = current.watcher();
	for (let i = 0; i < 4; i++) { t.mock.timers.tick(60_000); await pi.quiesce(); }
	assert.deepEqual(acksByCli, [id], "the new session confirms the acknowledgement the old one left unconfirmed");
	next.stdout.emit("data", Buffer.from(line(id) + "\n"));
	await pi.quiesce();
	assert.equal(current.sent.length, 0, "a message the old session committed is not handed to Pi again");
	assert.deepEqual(acksOf(next), [id], "its redelivery is acknowledged instead");
	await current.emit("session_shutdown", { reason: "quit" });
});

/// Scenario: Pi commits an inbox message whose acknowledgement the deck does not record at first, and then goes on committing a new message every half second. The first message is still confirmed with the deck CLI on its own schedule — asked at one second and again at its backoff — while the new ones keep arriving: fresh commits never push an older confirmation back.
test("pi_inbox_030_fresh_commits_do_not_postpone_an_older_confirmation", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const f = fixture(); const pi = admission(f);
	const [old, ...fresh] = idsN(9);
	const asked: string[] = [];
	f.pi.exec = async (_bin: string, argv: string[]) => {
		if (argv[0] !== "ack") return { code: 0, stdout: "", stderr: "", killed: false };
		const id = argv[argv.length - 1];
		asked.push(id);
		return id === old && asked.filter(a => a === old).length === 1
			? ackReply({ acknowledged: false, final: false, reason: "the deck is not reachable" })
			: ackReply({ acknowledged: true });
	};
	await f.emit("session_start"); const watcher = f.watcher();
	const commitOne = async (id: string) => {
		watcher.stdout.emit("data", Buffer.from(line(id) + "\n"));
		await pi.quiesce();
		if (pi.running) await pi.settle();
		await pi.quiesce();
		assert.equal(commitsOf(pi.committed, id), 1);
	};
	await commitOne(old);
	const askedOld = () => asked.filter(a => a === old).length;
	// A fresh commit every 500 ms for four seconds.
	for (const [i, id] of fresh.entries()) {
		t.mock.timers.tick(500); await pi.quiesce();
		const elapsed = (i + 1) * 500;
		if (elapsed === 1000) assert.equal(askedOld(), 1, `confirmed at one second, while new commits arrive: ${asked}`);
		if (elapsed === 3000) assert.equal(askedOld(), 2, `asked again at its two-second backoff: ${asked}`);
		await commitOne(id);
	}
	assert.equal(askedOld(), 2, "recorded on the second ask, and not asked again");
	for (let i = 0; i < 10; i++) { t.mock.timers.tick(60_000); await pi.quiesce(); }
	for (const id of fresh) assert.equal(asked.filter(a => a === id).length, 1, `each fresh id is confirmed once: ${asked}`);
	assert.equal(f.sent.length, 1 + fresh.length, "each message is handed to Pi once");
	pi.shutdown();
});
