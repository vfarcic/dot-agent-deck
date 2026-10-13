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
const { INBOX_ADMISSION_RECHECK_MS } = await import("../src/orchestrator.ts");
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
 * Pi 0.84.4's prompt admission, as `AgentSession.prompt` runs it: a send checks
 * for a running agent only after the input hooks resolve, a send that finds one
 * waits as a follow-up, and one that finds none runs its remaining preflight
 * before the agent takes it. A second send still in that preflight when another
 * prompt starts is refused by the agent ("Agent is already processing a
 * prompt"), and Pi reports the refusal itself: `sendUserMessage` returns before
 * it, so the caller never sees it.
 */
function admission(f: ReturnType<typeof fixture>) {
	let running = false;
	const followUps: string[] = [];
	const committed: string[] = [];
	const rejected: string[] = [];
	let refuseNext = 0;
	const tick = () => new Promise<void>(resolve => setImmediate(resolve));
	// Pi does not wait for the extension's card report before carrying on, and
	// the report's CLI is this file's mocked child, which never exits.
	const fire = (name: string, event: any = {}) => { void f.handlers.get(name)?.(event, f.ctx); };
	const commit = async (text: string) => {
		committed.push(text);
		fire("message_start", { message: { role: "user", content: [{ type: "text", text }] } });
		await tick();
	};
	const start = async (text: string) => { running = true; fire("agent_start"); await commit(text); };
	f.ctx.isIdle = () => !running;
	(f.ctx as any).hasPendingMessages = () => followUps.length > 0;
	f.pi.sendUserMessage = (text: string, options: any) => {
		f.sent.push({ text, options });
		void (async () => {
			await tick();
			if (refuseNext > 0) { refuseNext--; rejected.push(text); return; }
			if (running) { followUps.push(text); return; }
			await tick();
			if (running) { rejected.push(text); return; }
			await start(text);
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
		committed, rejected, tick, settle, quiesce,
		foreign: (text: string) => start(text),
		shutdown: () => fire("session_shutdown"),
		refuse: (n: number) => { refuseNext = n; },
		get running() { return running; },
	};
}

const idsN = (n: number) => Array.from({ length: n }, (_, i) => `m-k3xzq2ab-${String(i).repeat(8)}54b742c9`);
const line = (id: string) => JSON.stringify(delivery(BLOCK.replaceAll(ID, id), id));
const acksOf = (child: Watcher) => child.writes.flatMap(s => JSON.parse(s).ack as string[]);
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
