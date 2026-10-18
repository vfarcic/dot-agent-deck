/**
 * Tests the Pi extension's inbox glue against a stand-in watcher speaking the
 * deck's receipt protocol and a stand-in Pi with Pi 0.84.4's event contract
 * (PRD #1666). Pins named `pi_NNN` are run one at a time by the `inbox/pi/NNN`
 * wrappers in `src/orchestrator_ext.rs`.
 */
import assert from "node:assert/strict";
import childProcess from "node:child_process";
import { EventEmitter } from "node:events";
import { registerHooks, syncBuiltinESMExports } from "node:module";
import { afterEach, beforeEach, mock, test } from "node:test";

// The generated receiver runs only an absolute path to an executable file.
process.env.DOT_AGENT_DECK_EXE = process.execPath;
delete process.env.DOT_AGENT_DECK_BIN;
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
const { default: extension, DOORBELL_REGISTER_WAIT_MS, INBOX_UNSEEN_RECHECK_MS, INBOX_VERSION } = await import("../src/index.ts");
/** Another released receipt version than this module's, as a reload that changes version finds it. */
const OTHER_VERSION = INBOX_VERSION + 1;
hooks.deregister();

type Handler = (event: any, ctx: any) => unknown;
type Ref = { id: string; a: number; e: number };
const PROCESS = Symbol.for("dot-agent-deck.pi-receiver");
const ID = "m-k3xzq2ab-173ce9ac54b742c9";
const ID2 = "m-k3xzq2ab-bbbbbbbb54b742c9";
const block = (id = ID) => `[dot-agent-deck message ${id} from the orchestrator]\nWrite a sentinel.\n[end of dot-agent-deck message ${id}]`;
const ref = (id = ID, a = 1, e = 1): Ref => ({ id, a, e });
const settle = async (rounds = 6) => {
	for (let i = 0; i < rounds; i++) await new Promise((resolve) => setImmediate(resolve));
};

/** A watcher as the receiver sees it: lines in on stdout, controls out on stdin. */
class Watcher extends EventEmitter {
	stdout = Object.assign(new EventEmitter(), { setEncoding: (_encoding: string) => this.stdout });
	pid: number;
	lines: any[] = [];
	ended = false;
	signals: string[] = [];
	exitCode: number | null = null;
	signalCode: string | null = null;
	/** Answer every `may` at once: `true` grants, `false` refuses, `undefined` leaves it unanswered. */
	grant: boolean | undefined = true;
	stdin = {
		writableEnded: false,
		destroyed: false,
		write: (line: string) => {
			const control = JSON.parse(String(line));
			this.lines.push(control);
			if (control.may && this.grant !== undefined) {
				const answer = this.grant;
				queueMicrotask(() => this.say({ grant: { q: control.may.q, ok: answer } }));
			}
			return true;
		},
		end: () => { this.ended = true; this.stdin.writableEnded = true; },
		on: () => this.stdin,
	};
	constructor(pid: number) { super(); this.pid = pid; }
	kill(signal = "SIGTERM") { this.signals.push(signal); return true; }
	say(reply: unknown) { this.stdout.emit("data", Buffer.from(JSON.stringify(reply) + "\n")); }
	register(rx = 7, extra: Record<string, unknown> = {}) { this.say({ registered: { rx, keepalive_ms: 1000 }, ...extra }); }
	hand(...refs: Ref[]) { this.say({ messages: refs.map((r, i) => ({ ...r, d: i + 1, text: block(r.id) })) }); }
	exit(code: number | null = 1) { this.exitCode = code; this.emit("exit", code, null); }
	/** The controls of `kind` the receiver wrote. */
	sent(kind: string): any[] { return this.lines.filter((l) => kind in l).map((l) => l[kind]); }
	/** The refs every control of `kind` carried, flattened. */
	refs(kind: string): Ref[] { return this.sent(kind).flat().map((r: Ref) => ({ id: r.id, a: r.a, e: r.e })); }
}

let spawned: { bin: string; argv: string[]; options: any; child: Watcher }[];
beforeEach(() => {
	freshProcess();
	mock.method(childProcess, "spawn", (bin: string, argv: string[], options: any) => {
		const child = new Watcher(41000 + spawned.length);
		if (argv.includes("wait")) {
			spawned.push({ bin, argv, options, child });
		} else {
			// A report the CLI is spawned for (a turn's reply on stdin)
			// finishes, as the real CLI does.
			setImmediate(() => child.emit("close", 0, null));
		}
		return child;
	});
	syncBuiltinESMExports();
});
afterEach(() => {
	const proc = (globalThis as any)[PROCESS];
	if (proc) proc.disposed = true;
	mock.restoreAll();
	syncBuiltinESMExports();
});

/** One extension instance on a stand-in Pi. */
function instance() {
	const handlers = new Map<string, Handler>();
	const sends: { text: string; options: any }[] = [];
	const execs: string[][] = [];
	const ctx: any = { cwd: "/scratch/pi-inbox", signal: undefined, isIdle: () => true, hasPendingMessages: () => false };
	/** When set, Pi runs the `input` gate inside `sendUserMessage`, as it does for an idle agent. */
	let gateInline = false;
	const gated: unknown[] = [];
	const pi: any = {
		registerTool() {},
		on(name: string, handler: Handler) { handlers.set(name, handler); },
		exec: async (_bin: string, argv: string[]) => { execs.push(argv); return { code: 0, stdout: "", stderr: "", killed: false }; },
		sendUserMessage(text: string, options: any) {
			sends.push({ text, options });
			if (gateInline) void Promise.resolve(handlers.get("input")!({ text, source: "extension" }, ctx)).then((a: unknown) => gated.push(a));
			return undefined;
		},
	};
	extension(pi);
	const emit = async (name: string, event: any = {}) => handlers.get(name)?.(event, ctx);
	/** Run the gate on the hand-over `i` sent, as Pi's `input` hooks would. */
	const gate = (i = sends.length - 1) => emit("input", { text: sends[i].text, source: "extension" }) as Promise<any>;
	/** The user message carrying `ids` started: their commit. */
	const start = (...ids: string[]) => emit("message_start", { message: { role: "user", content: [{ type: "text", text: ids.map(block).join("\n\n") }] } });
	return { handlers, sends, execs, ctx, pi, emit, gate, start, gated, inline: (on: boolean) => { gateInline = on; } };
}

/** The watcher the process's receiver is running now. */
const watcher = () => {
	const found = spawned.slice().reverse().find((s) => s.argv.includes("wait"));
	assert.ok(found, "the receiver runs an inbox wait --stream watcher");
	return found.child;
};

/** Forget the process's receiver, as a new Pi process would start without one. */
function freshProcess() {
	const old = (globalThis as any)[PROCESS];
	if (old) { old.disposed = true; for (const t of old.rechecks?.values() ?? []) clearTimeout(t); }
	delete (globalThis as any)[PROCESS];
	spawned = [];
}

/** A new Pi process's first session, whose watcher registered. */
async function started(reason = "startup") {
	freshProcess();
	const pi = instance();
	await pi.emit("session_start", { reason });
	const child = watcher();
	child.register();
	await settle();
	return { pi, child };
}

/// Scenario: Pi's first session starts the receiver's watcher on the receipt stream at the deck's adapter integration, and a respawned watcher registers as resumed. A doorbell fetch names that integration and the receiver it is for.
test("pi_005 the watcher and the doorbell fetch declare the receipt integration", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const { pi, child } = await started();
	assert.deepEqual(spawned[0].argv, ["inbox", "wait", "--agent", "pi", "--stream", "--adapter-integration", "2"]);
	assert.equal(spawned[0].options.stdio[0], "pipe", "EOF must end an orphaned watcher");
	child.exit(1);
	t.mock.timers.tick(1000);
	assert.deepEqual(spawned[1].argv, ["inbox", "wait", "--agent", "pi", "--stream", "--adapter-integration", "2", "--resumed"]);
	watcher().register(9);
	await settle();
	pi.pi.exec = async (_bin: string, argv: string[]) => { pi.execs.push(argv); return { code: 0, stdout: JSON.stringify({ messages: [] }), stderr: "", killed: false }; };
	await pi.emit("input", { text: `dot-agent-deck: message ${ID} waiting, run: dot-agent-deck inbox fetch`, source: "interactive" });
	assert.deepEqual(pi.execs.find((argv) => argv[1] === "fetch"), ["inbox", "fetch", "--json", "--doorbell", ID, "--adapter-integration", "2", "--rx", "9"]);
});

/// Scenario: The deck hands one attempt to Pi's receiver, then the same attempt again, then a marked repeat of it. Pi gets one follow-up for the attempt and one for the repeat, each behind the deck's approval.
test("pi_007 a repeated hand-out is sent once, and a marked repeat is its own attempt", async () => {
	const { pi, child } = await started();
	child.hand(ref());
	await settle();
	child.hand(ref());
	await settle();
	assert.equal(pi.sends.length, 1, "the same attempt is handed to Pi once");
	assert.deepEqual(pi.sends[0].options, { deliverAs: "followUp" });
	assert.ok(pi.sends[0].text.includes(block()));
	assert.deepEqual(child.refs("may"), [ref()], "asked once, before the call");
	child.hand(ref(ID, 1, 2));
	await settle();
	child.hand(ref(ID, 1, 2));
	await settle();
	assert.equal(pi.sends.length, 2, "the marked repeat is its own attempt, sent once");
});

/// Scenario: An attempt is submitted to Pi but no gate has seen it when the session is replaced by /new. The successor submits the same attempt again behind a fresh approval; the first submission's tag is consumed at either instance's gate, and the attempt passes a gate once.
test("pi_007 a submission no gate saw is submitted again to a /new successor, and passes one gate", async () => {
	const { pi, child } = await started();
	child.hand(ref());
	await settle();
	const first = pi.sends[0].text;
	await pi.emit("session_shutdown", { reason: "new" });
	const next = instance();
	await next.emit("session_start", { reason: "new" });
	await settle();
	assert.equal(pi.sends.length + next.sends.length, 2, "two sendUserMessage calls for one attempt");
	assert.equal(next.sends.length, 1);
	assert.deepEqual(child.refs("may"), [ref(), ref()], "each behind its own approval");
	assert.deepEqual(await pi.emit("input", { text: first, source: "extension" }), { action: "handled" });
	assert.deepEqual(await next.emit("input", { text: first, source: "extension" }), { action: "handled" });
	assert.equal((await next.gate()).action, "transform");
	assert.deepEqual(child.refs("handed"), [ref()], "one hand-over past a gate");
	assert.deepEqual(child.refs("not_handed"), []);
});

/// Scenario: Hand-overs reach the gate after the deck revoked the attempt, after the watcher restarted, after the approval went stale, and twice. Each is consumed rather than passed to the model; the restarted and stale ones are reported not handed, and a revoked one is not reported a second time.
test("pi_008 the gate consumes refused hand-overs and reports not handed after revoking", async (t) => {
	// Revoked before the gate: the receiver answered revoked_ack.
	{
		const { pi, child } = await started();
		child.hand(ref());
		await settle();
		child.say({ revoke: [ref()] });
		assert.deepEqual(child.refs("revoked_ack"), [ref()]);
		assert.deepEqual(await pi.gate(), { action: "handled" });
		assert.deepEqual(child.refs("not_handed"), [], "the revoke's answer stands alone");
	}
	// The watcher restarted between the call and the gate (a rebind).
	{
		t.mock.timers.enable({ apis: ["setTimeout"] });
		const { pi, child } = await started();
		child.hand(ref());
		await settle();
		child.exit(1);
		t.mock.timers.tick(1000);
		const next = watcher();
		next.register(8, { resume: [ref()] });
		await settle();
		assert.deepEqual(await pi.gate(), { action: "handled" });
		assert.deepEqual(next.refs("not_handed"), [ref()]);
		assert.deepEqual(await pi.gate(), { action: "handled" }, "a duplicate input is consumed too");
		assert.deepEqual(next.refs("not_handed"), [ref()], "once");
		t.mock.timers.reset();
	}
	// A stale approval (older than the deck's retirement bound).
	{
		const { pi, child } = await started();
		child.hand(ref());
		await settle();
		const now = performance.now();
		const clock = mock.method(performance, "now", () => now + 60_000);
		assert.deepEqual(await pi.gate(), { action: "handled" });
		clock.mock.restore();
		assert.deepEqual(child.refs("not_handed"), [ref()]);
	}
	// Passed once; the same hand-over again is consumed.
	{
		const { pi, child } = await started();
		child.hand(ref());
		await settle();
		assert.equal((await pi.gate()).action, "transform");
		assert.deepEqual(await pi.gate(), { action: "handled" });
		assert.deepEqual(child.refs("handed"), [ref()]);
		assert.deepEqual(child.refs("not_handed"), []);
	}
});

/// Scenario: A gate whose check throws consumes the hand-over instead of passing the tagged text to the model, and a user message carrying our block that reached Pi around the gate commits the attempt.
test("pi_009 the gate fails closed and our opener without our tag commits", async () => {
	const { pi, child } = await started();
	child.hand(ref());
	await settle();
	const proc = (globalThis as any)[PROCESS];
	const calling = proc.receiver.calling;
	proc.receiver.calling = () => { throw new Error("broken gate"); };
	assert.deepEqual(await pi.gate(), { action: "handled" }, "a throwing gate consumes");
	proc.receiver.calling = calling;
	child.hand(ref(ID2));
	await settle();
	await pi.start(ID2);
	assert.deepEqual(child.refs("committed"), [ref(ID2)], "content that reached Pi around the gate commits");
});

/// Scenario: A hand-over is sent but Pi's gate never sees it. A revoke is answered revoked_ack and the late gate arrival is consumed; with no revoke, once Pi is idle with nothing queued the recheck reports it not handed, and a repeated revoke gets the same answer with no second not handed.
test("pi_010 a sent attempt the gate never saw", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	{
		const { pi, child } = await started();
		child.hand(ref());
		await settle();
		child.say({ revoke: [ref()] });
		child.say({ revoke: [ref()] });
		assert.deepEqual(child.refs("revoked_ack"), [ref(), ref()]);
		assert.deepEqual(await pi.gate(), { action: "handled" });
		t.mock.timers.tick(INBOX_UNSEEN_RECHECK_MS);
		assert.deepEqual(child.refs("not_handed"), []);
	}
	{
		const { pi, child } = await started();
		pi.ctx.isIdle = () => false;
		child.hand(ref());
		await settle();
		t.mock.timers.tick(INBOX_UNSEEN_RECHECK_MS);
		assert.deepEqual(child.refs("not_handed"), [], "Pi busy: it may still reach the gate");
		pi.ctx.isIdle = () => true;
		t.mock.timers.tick(INBOX_UNSEEN_RECHECK_MS);
		assert.deepEqual(child.refs("not_handed"), [ref()]);
		assert.deepEqual(await pi.gate(), { action: "handled" }, "its late arrival is consumed");
		child.say({ revoke: [ref()] });
		child.say({ revoke: [ref()] });
		assert.deepEqual(child.refs("not_handed"), [ref()], "no second not handed");
	}
});

/// Scenario: Attempts pass the gate and then a revoke, Pi's refusal pattern, an Escape and a settle in preflight arrive. A revoke is answered already_handed; the refusal and the Escape release the attempt lost, never not handed; a settle while it is still in preflight leaves it held.
test("pi_011 after the gate: already handed, lost on positive settle evidence, held in preflight", async () => {
	// Revoke after the gate.
	{
		const { pi, child } = await started();
		child.hand(ref());
		await settle();
		await pi.gate();
		child.say({ revoke: [ref()] });
		assert.deepEqual(child.refs("already_handed"), [ref()]);
	}
	// Refusal: before_agent_start saw it, a settle while another run is active, then one with none.
	{
		const { pi, child } = await started();
		child.hand(ref());
		await settle();
		await pi.gate();
		await pi.emit("before_agent_start", { prompt: block() });
		pi.ctx.signal = new AbortController().signal;
		await pi.emit("agent_settled", {});
		assert.deepEqual(child.sent("possess").at(-1).held, [ref()], "still held while the other run is active");
		pi.ctx.signal = undefined;
		pi.ctx.hasPendingMessages = () => true;
		await pi.emit("agent_settled", {});
		assert.deepEqual(child.sent("possess").at(-1).held, [], "released lost");
		assert.deepEqual(child.refs("not_handed"), [], "never not handed");
		child.say({ revoke: [ref()] });
		assert.deepEqual(child.refs("already_handed"), [ref()], "still possibly handed");
	}
	// Escape: a run started after it passed, and Pi settles idle with nothing queued.
	{
		const { pi, child } = await started();
		child.hand(ref());
		await settle();
		await pi.gate();
		await pi.emit("agent_start", {});
		await pi.emit("agent_settled", {});
		assert.deepEqual(child.sent("possess").at(-1).held, []);
		assert.deepEqual(child.refs("not_handed"), []);
	}
	// A settle while it is in preflight: no run started since it passed.
	{
		const { pi, child } = await started();
		child.hand(ref());
		await settle();
		await pi.gate();
		await pi.emit("agent_settled", {});
		assert.deepEqual(child.sent("possess").at(-1).held, [ref()]);
	}
});

/// Scenario: Two attempts of one message pass the gate, and the first is released lost before the user message carrying the message starts. The start commits both, the released one included; a revoke that races the commit's publication is answered already handed.
test("pi_012 message_start commits every called attempt of the id", async () => {
	const { pi, child } = await started();
	child.hand(ref(ID, 1, 1));
	await settle();
	await pi.gate();
	await pi.emit("agent_start", {});
	await pi.emit("agent_settled", {});
	child.hand(ref(ID, 1, 2));
	await settle();
	await pi.gate();
	await pi.start(ID);
	assert.deepEqual(child.refs("committed"), [ref(ID, 1, 1), ref(ID, 1, 2)]);
	child.say({ revoke: [ref(ID, 1, 2)] });
	assert.deepEqual(child.refs("already_handed"), [ref(ID, 1, 2)]);
});

/// Scenario: The receiver's possession snapshots number themselves, carry the contiguous watermark, and list a handed attempt until its commit, the gate's pass included; a keepalive brings a fresh snapshot.
test("pi_013 snapshots hold an attempt until its commit", async () => {
	const { pi, child } = await started();
	child.hand(ref());
	await settle();
	await pi.gate();
	const after = child.sent("possess").at(-1);
	assert.deepEqual(after.held, [ref()], "passing the gate does not drop possession");
	assert.equal(after.seen, 1);
	const count = child.sent("possess").length;
	child.say({ keepalive: true });
	assert.equal(child.sent("possess").length, count + 1);
	assert.ok(child.sent("possess").at(-1).seq > after.seq);
	await pi.start(ID);
	assert.deepEqual(child.sent("possess").at(-1).held, []);
});

/// Scenario: A session shuts down and a new one starts in the same Pi process. The watcher's stdin stays open, it is not killed, and the new session does not register a second watcher; it is handed the next message.
test("pi_014 a session replacement keeps the one watcher", async () => {
	const { pi, child } = await started();
	await pi.emit("session_shutdown", { reason: "new" });
	assert.equal(child.ended, false, "session_shutdown leaves the watcher's stdin open");
	assert.deepEqual(child.signals, [], "and does not signal it");
	const next = instance();
	await next.emit("session_start", { reason: "new" });
	assert.equal(spawned.length, 1, "no second registration");
	child.hand(ref());
	await settle();
	assert.equal(next.sends.length, 1);
	assert.equal(pi.sends.length, 0);
});

/// Scenario: The watcher dies with one attempt held and another reported committed while no watcher ran. The respawn answers both resumes retained and the replacement publishes the commit; a commit of a Cancelled attempt is sent again only while the deck lists it correctable, across two respawns.
test("pi_015 respawn: resumes retained, reports republished, correctable re-sent", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const { pi, child } = await started();
	child.hand(ref(ID, 1, 1), ref(ID2, 1, 1));
	await settle();
	pi.gate(0);
	await settle();
	child.exit(1);
	await pi.start(ID);
	t.mock.timers.tick(1000);
	const second = watcher();
	second.register(8, { resume: [ref(ID, 1, 1), ref(ID2, 1, 1)] });
	assert.deepEqual(second.sent("resume_answers").flat().map((a: any) => a.answer), ["retained", "retained"]);
	assert.deepEqual(second.refs("committed"), [ref(ID, 1, 1)], "published by the replacement");
	for (const n of [9, 10]) {
		second.exit(1);
		t.mock.timers.tick(30000);
		const next = watcher();
		next.register(n, { correctable: [ref(ID, 1, 1)] });
		assert.deepEqual(next.refs("committed"), [ref(ID, 1, 1)], `re-sent under correctable to watcher ${n}`);
	}
	const last = watcher();
	last.exit(1);
	t.mock.timers.tick(30000);
	watcher().register(11);
	assert.deepEqual(watcher().refs("committed"), [], "not without correctable");
});

/// Scenario: A doorbell line is submitted while the watcher is registered, is recalled, arrives just before the watcher registers, and arrives with no registration at all. Registered, or registering within the wait, it is fetched to this receiver and transformed in place; recalled, it is consumed; unregistered past the wait, it is consumed and the mail stays queued.
test("pi_016 doorbell: fetched to the receiver and transformed in place", async (t) => {
	const line = `dot-agent-deck: message ${ID} waiting, run: dot-agent-deck inbox fetch`;
	{
		const { pi, child } = await started();
		pi.pi.exec = async (_bin: string, argv: string[]) => {
			pi.execs.push(argv);
			const messages = argv[1] === "fetch" ? [{ ...ref(), d: 1, text: block() }] : [];
			return { code: 0, stdout: JSON.stringify({ messages }), stderr: "", killed: false };
		};
		const answer: any = await pi.emit("input", { text: line, source: "interactive" });
		assert.equal(answer.action, "transform");
		assert.ok(answer.text.includes(block()));
		assert.deepEqual(child.refs("handed"), [ref()]);
		assert.equal(pi.sends.length, 0, "transformed in place, not sent again");
		await pi.start(ID);
		assert.deepEqual(child.refs("committed"), [ref()]);
	}
	{
		const { pi } = await started();
		pi.pi.exec = async () => ({ code: 0, stdout: JSON.stringify({ messages: [] }), stderr: "", killed: false });
		assert.deepEqual(await pi.emit("input", { text: line, source: "interactive" }), { action: "handled" });
	}
	{
		// Registration arrives during the wait: then it is fetched.
		freshProcess();
		const pi = instance();
		pi.pi.exec = async (_bin: string, argv: string[]) => {
			pi.execs.push(argv);
			const messages = argv[1] === "fetch" ? [{ ...ref(), d: 1, text: block() }] : [];
			return { code: 0, stdout: JSON.stringify({ messages }), stderr: "", killed: false };
		};
		await pi.emit("session_start", { reason: "startup" });
		const answer = pi.emit("input", { text: line, source: "interactive" });
		await settle();
		watcher().register(5);
		assert.equal(((await answer) as any).action, "transform");
		assert.ok(pi.execs.some((argv) => argv[1] === "fetch" && argv.at(-1) === "5"));
	}
	{
		t.mock.timers.enable({ apis: ["setTimeout", "Date"] });
		freshProcess();
		const pi = instance();
		await pi.emit("session_start", { reason: "startup" });
		const answer = pi.emit("input", { text: line, source: "interactive" });
		for (let waited = 0; waited <= DOORBELL_REGISTER_WAIT_MS; waited += 100) {
			await settle(2);
			t.mock.timers.tick(100);
		}
		assert.deepEqual(await answer, { action: "handled" });
		assert.ok(!pi.execs.some((argv) => argv[1] === "fetch"), "nothing fetched: the mail stays queued");
	}
});

/// Scenario: Pi's session shuts down for /new with one attempt past the gate and uncommitted and another sent but unseen. The first is released lost and stays possibly handed; the second goes uncalled to the successor; a commit the outgoing session made while the watcher restarted is still published after the swap.
test("pi_024 a /new swap releases called-uncommitted attempts lost", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const { pi, child } = await started();
	child.hand(ref(ID, 1, 1), ref(ID2, 1, 1));
	await settle();
	await pi.gate(0);
	child.exit(1);
	await pi.start(ID2);
	await pi.emit("session_shutdown", { reason: "new" });
	assert.deepEqual(child.refs("not_handed"), []);
	const next = instance();
	await next.emit("session_start", { reason: "new" });
	t.mock.timers.tick(1000);
	const second = watcher();
	second.register(8, { resume: [ref(ID, 1, 1), ref(ID2, 1, 1)] });
	await settle();
	assert.deepEqual(second.refs("committed"), [ref(ID2, 1, 1)], "the outgoing session's commit is published");
	assert.deepEqual(second.sent("possess").at(-1).held, [], "the called attempt was released lost");
	second.say({ revoke: [ref(ID, 1, 1)] });
	assert.deepEqual(second.refs("already_handed"), [ref(ID, 1, 1)]);
	assert.equal(next.sends.length, 0, "a released attempt is not handed to the successor");
});

/// Scenario: Each of /new, resume, fork and reload is driven through a stand-in Pi with one attempt called and uncommitted and one held uncalled. The first three release the called one lost and hand the held one to the successor; reload releases nothing and a later start in the successor commits it; no restored opener commits; one watcher throughout.
test("pi_030 each session replacement in Pi 0.84.4's order", async () => {
	for (const reason of ["new", "resume", "fork", "reload"]) {
		const { pi, child } = await started();
		pi.ctx.isIdle = () => false;
		child.hand(ref(ID, 1, 1));
		await settle();
		await pi.gate(0);
		child.grant = undefined;
		child.hand(ref(ID2, 1, 1));
		await settle();
		await pi.emit("session_shutdown", { reason });
		await pi.start(ID);
		assert.deepEqual(child.refs("committed"), [], `${reason}: no commit from a shut-down instance`);
		child.grant = true;
		child.say({ grant: { q: child.sent("may").at(-1).q, ok: true } });
		const next = instance();
		await next.emit("session_start", { reason });
		await settle();
		if (reason === "reload") {
			assert.ok(child.sent("possess").at(-1).held.some((r: Ref) => r.id === ID), "reload keeps the called attempt held");
			await next.start(ID);
			assert.deepEqual(child.refs("committed"), [ref(ID, 1, 1)], "the successor's start commits it");
		} else {
			assert.ok(!child.sent("possess").at(-1).held.some((r: Ref) => r.id === ID), `${reason}: released lost`);
			assert.equal(next.sends.length, 1, `${reason}: the held one goes to the successor`);
			assert.ok(next.sends[0].text.includes(block(ID2)));
		}
		assert.equal(spawned.length, 1, `${reason}: the same watcher`);
	}
});

/// Scenario: Pi's watcher is registered and no session is current. A message is taken into the receiver and held, not handed to Pi; the next session is handed it through a fresh approval; with no successor it stays held, and a Cancel is answered revoked_ack.
test("pi_025 no current session: held in pending until a successor", async () => {
	{
		const { pi, child } = await started();
		await pi.emit("session_shutdown", { reason: "new" });
		child.hand(ref());
		await settle();
		assert.deepEqual(child.sent("possess").at(-1).held, [ref()]);
		assert.deepEqual(child.refs("may"), [], "nothing asked while no session is current");
		const next = instance();
		await next.emit("session_start", { reason: "new" });
		await settle();
		assert.deepEqual(child.refs("may"), [ref()]);
		assert.equal(next.sends.length, 1);
	}
	{
		const { pi, child } = await started();
		await pi.emit("session_shutdown", { reason: "quit" });
		child.hand(ref());
		await settle();
		child.say({ revoke: [ref()] });
		assert.deepEqual(child.refs("revoked_ack"), [ref()]);
		assert.equal(pi.sends.length, 0);
	}
});

/// Scenario: A hand-over approved for one session resumes after a successor took over, a session's delayed shutdown arrives after its successor started, and the outgoing session reports Thinking after it was replaced. The approval does not reach the successor's Pi and the attempt goes to it uncalled; the delayed shutdown leaves the successor current; the stale Thinking is not reported while the outgoing Idle is, in order.
test("pi_026 session tokens: a delayed approval, shutdown and report", async () => {
	// The approval arrives after the swap.
	{
		const { pi, child } = await started();
		child.grant = undefined;
		child.hand(ref());
		await settle();
		await pi.emit("session_shutdown", { reason: "new" });
		const next = instance();
		await next.emit("session_start", { reason: "new" });
		child.say({ grant: { q: child.sent("may")[0].q, ok: true } });
		child.grant = true;
		await settle();
		assert.equal(pi.sends.length, 0, "the old session's pi is never called");
		assert.equal(next.sends.length, 1, "the successor gets it after its own approval");
		assert.equal(child.refs("may").length, 2);
	}
	// A delayed shutdown leaves the successor current.
	{
		const { pi, child } = await started();
		const next = instance();
		await next.emit("session_start", { reason: "new" });
		await pi.emit("session_shutdown", { reason: "new" });
		child.hand(ref());
		await settle();
		assert.equal(next.sends.length, 1);
	}
	// A stale report is dropped; the outgoing Idle keeps its place.
	{
		const { pi } = await started();
		const reports: string[][] = [];
		const record = async (_bin: string, argv: string[]) => { reports.push(argv); return { code: 0, stdout: "", stderr: "", killed: false }; };
		pi.pi.exec = record;
		await pi.emit("session_shutdown", { reason: "new" });
		const next = instance();
		next.pi.exec = record;
		await next.emit("session_start", { reason: "new" });
		await pi.emit("agent_start", {});
		const types = reports.filter((a) => a[0] === "agent-event").map((a) => a[a.indexOf("--type") + 1]);
		assert.deepEqual(types, ["finished", "finished"], "the outgoing Idle, then the successor's; no stale running");
	}
});

/// Scenario: A session's shutdown is delayed until its successor has started a turn (Thinking reported) and been handed a follow-up. The outgoing Idle is not reported, so it cannot end the successor's turn, and the follow-up stays held.
test("pi_026 a delayed shutdown after the successor's Thinking reports no Idle", async () => {
	const { pi, child } = await started();
	const types: string[] = [];
	const record = async (_bin: string, argv: string[]) => {
		if (argv[0] === "agent-event") types.push(argv[argv.indexOf("--type") + 1]);
		return { code: 0, stdout: "", stderr: "", killed: false };
	};
	pi.pi.exec = record;
	const next = instance();
	next.pi.exec = record;
	next.ctx.isIdle = () => false;
	await next.emit("session_start", { reason: "new" });
	await next.emit("before_agent_start", { prompt: "keep working" });
	await next.emit("agent_start", {});
	child.hand(ref());
	await settle();
	assert.equal(next.sends.length, 1, "the follow-up is the successor's");
	await pi.emit("session_shutdown", { reason: "new" });
	await settle();
	assert.deepEqual(types, ["finished", "prompt", "running"], "no Idle after the successor's Thinking");
	assert.deepEqual(child.sent("possess").at(-1).held, [ref()], "the follow-up stays held");
});

/// Scenario: A session passes an attempt through the gate and is replaced by /new, its shutdown arriving only after the successor started. The outgoing attempt is still released lost, and the successor's settle reports nothing not handed for it; a sent attempt the gate never saw is submitted again to the successor.
test("pi_026 a delayed /new shutdown still releases the outgoing session's attempts", async () => {
	const { pi, child } = await started();
	child.hand(ref(ID));
	await settle();
	await pi.gate();
	child.hand(ref(ID2));
	await settle();
	assert.equal(pi.sends.length, 2, "the second is sent and unseen by the gate");
	const next = instance();
	await next.emit("session_start", { reason: "new" });
	await pi.emit("session_shutdown", { reason: "new" });
	await settle();
	assert.deepEqual(child.sent("possess").at(-1).held, [ref(ID2)], "the passed attempt is released lost");
	assert.equal(next.sends.length, 1, "the unseen one goes to the successor");
	assert.ok(next.sends[0].text.includes(block(ID2)));
	await next.gate();
	await next.start(ID2);
	await next.emit("agent_start", {});
	await next.emit("agent_settled", {});
	assert.deepEqual(child.refs("committed"), [ref(ID2)]);
	child.say({ revoke: [ref(ID)] });
	assert.deepEqual(child.refs("already_handed"), [ref(ID)], "possibly handed");
	assert.deepEqual(child.refs("not_handed"), []);
});

/// Scenario: A session passes an attempt through the gate and reloads, its shutdown arriving only after the reload's successor started. The successor owns the attempt: a settle after a run with nothing queued releases it lost.
test("pi_026 a delayed reload shutdown hands what passed to the successor", async () => {
	const { pi, child } = await started();
	child.hand(ref());
	await settle();
	await pi.gate();
	const next = instance();
	await next.emit("session_start", { reason: "reload" });
	await pi.emit("session_shutdown", { reason: "reload" });
	await settle();
	assert.deepEqual(child.sent("possess").at(-1).held, [ref()], "a reload releases nothing");
	await next.emit("agent_start", {});
	await next.emit("agent_settled", {});
	assert.deepEqual(child.sent("possess").at(-1).held, [], "the successor released it lost");
	assert.deepEqual(child.refs("not_handed"), []);
});

/// Scenario: Inputs carrying a hand-over tag this process never issued, a tag from an older version's format and a well-formed tag for an unknown attempt reach the gate. Each is consumed, and nothing is reported to the deck.
test("pi_027 unknown or older tags are consumed silently", async () => {
	const { pi, child } = await started();
	for (const text of [`[dot-agent-deck hand-over zzzz.1]\n${block()}`, `[dot-agent-deck hand-over abc.99]\n${block()}`]) {
		assert.deepEqual(await pi.emit("input", { text, source: "extension" }), { action: "handled" });
	}
	assert.deepEqual(child.lines.filter((l) => !("possess" in l)), []);
});

/// Scenario: A reload evaluates a different version of the extension while the old receiver holds attempts. The old receiver is disposed before its watcher is drained, the drain is awaited (its exit, or forced at the deadline), two initializers share one replacement, compatible state is carried and answered retained, an incompatible one answers unknown and never not handed, and the old receiver's callbacks deliver nothing after disposal.
test("pi_028 a version replacement: dispose, drain, transfer", async (t) => {
	// Version 1 was never released: its state is not carried even in this shape.
	for (const [version, stateVersion, compatible] of [[OTHER_VERSION, undefined, true], [OTHER_VERSION, 0, false], [1, undefined, false]] as const) {
		const { pi, child } = await started();
		child.hand(ref());
		await settle();
		await pi.gate();
		const old = (globalThis as any)[PROCESS];
		old.version = version;
		if (stateVersion !== undefined) old.stateVersion = stateVersion;
		const order: string[] = [];
		const drain = old.receiver.drain;
		old.receiver.drain = () => { order.push(old.disposed ? "drain after dispose" : "drain before dispose"); return drain(); };
		await pi.emit("session_shutdown", { reason: "reload" });
		const a = instance();
		const b = instance();
		const starts = [a.emit("session_start", { reason: "reload" }), b.emit("session_start", { reason: "reload" })];
		await settle();
		assert.equal(child.ended, true, "the old watcher's stdin is ended");
		assert.equal(spawned.length, 1, "no successor while the old watcher runs");
		const before = pi.sends.length;
		child.hand(ref(ID2));
		await settle();
		assert.equal(a.sends.length + b.sends.length + pi.sends.length, before, "the disposed receiver delivers nothing");
		child.exit(0);
		await Promise.all(starts);
		assert.deepEqual(order, ["drain after dispose"]);
		assert.equal(spawned.length, 2, "one replacement for two initializers");
		const next = watcher();
		assert.ok(spawned[1].argv.includes("--resumed"));
		next.register(8, { resume: [ref()] });
		const answer = next.sent("resume_answers").flat()[0].answer;
		assert.equal(answer, compatible ? "retained" : "unknown");
		next.say({ revoke: [ref()] });
		assert.deepEqual(next.refs("already_handed"), [ref()], "possibly handed, never not handed");
		assert.deepEqual(next.refs("not_handed"), []);
	}
	// A drain that the watcher does not answer is forced at the deadline.
	{
		t.mock.timers.enable({ apis: ["setTimeout"] });
		const { child } = await started();
		const old = (globalThis as any)[PROCESS];
		old.version = OTHER_VERSION;
		const next = instance();
		const start = next.emit("session_start", { reason: "reload" });
		await settle();
		t.mock.timers.tick(3000);
		assert.deepEqual(child.signals, ["SIGTERM"]);
		t.mock.timers.tick(2000);
		assert.deepEqual(child.signals, ["SIGTERM", "SIGKILL"]);
		await start;
		assert.equal(spawned.length, 2);
	}
});

const ID3 = "m-k3xzq2ab-cccccccc54b742c9";

/**
 * A reload of a different version: `pi` shuts down for the reload, and its
 * successor's session_start waits while the old receiver drains. `finish`
 * lets the old watcher exit and resolves once the successor is current.
 */
async function replacing(pi: ReturnType<typeof instance>, child: Watcher) {
	(globalThis as any)[PROCESS].version = OTHER_VERSION;
	await pi.emit("session_shutdown", { reason: "reload" });
	const next = instance();
	const start = next.emit("session_start", { reason: "reload" });
	await settle();
	assert.equal(child.ended, true, "the old watcher is draining");
	const finish = async () => {
		child.exit(0);
		await start;
		assert.equal(spawned.length, 2);
		return watcher();
	};
	return { next, finish };
}

/// Scenario: While a version replacement drains the old watcher, that watcher hands out a message and a revoke, and the reload's successor starts on an attempt that passed the gate. Nothing reaches Pi during the drain; afterwards the successor's receiver hands the message to the successor, answers the re-sent revoke revoked_ack, and publishes the commit made during the drain.
test("pi_028 drain: the outgoing receiver keeps reading its watcher until it exits", async () => {
	const { pi, child } = await started();
	child.hand(ref(ID));
	await settle();
	await pi.gate();
	const { next, finish } = await replacing(pi, child);
	child.hand(ref(ID2), ref(ID3));
	child.say({ revoke: [ref(ID3)] });
	await next.start(ID);
	await settle();
	assert.equal(pi.sends.length + next.sends.length, 1, "nothing handed to Pi during the drain");
	const second = await finish();
	second.register(8, { resume: [ref(ID), ref(ID2)] });
	await settle();
	assert.deepEqual(second.sent("resume_answers").flat().map((r: any) => r.answer), ["retained", "retained"]);
	assert.deepEqual(second.refs("committed"), [ref(ID)], "the commit made during the drain is published");
	assert.equal(next.sends.length, 1, "the message taken during the drain goes to the successor");
	assert.ok(next.sends[0].text.includes(block(ID2)));
	second.say({ revoke: [ref(ID3)] });
	assert.deepEqual(second.refs("revoked_ack"), [ref(ID3)], "the revoke the drain took stands");
	assert.deepEqual(second.refs("not_handed"), []);
});

/// Scenario: A version replacement starts while one attempt waits for the deck's approval and a second is queued behind it. Neither is lost: once the replacement's watcher registers, the successor is handed both, in order, each behind a fresh approval.
test("pi_028 a replacement carries a hand-over waiting for approval and the queue behind it", async () => {
	const { pi, child } = await started();
	child.grant = undefined;
	child.hand(ref(ID), ref(ID2));
	await settle();
	assert.deepEqual(child.refs("may"), [ref(ID)], "the second waits behind the first");
	const { next, finish } = await replacing(pi, child);
	const second = await finish();
	second.register(8, { resume: [ref(ID), ref(ID2)] });
	await settle();
	assert.deepEqual(second.sent("resume_answers").flat().map((r: any) => r.answer), ["retained", "retained"]);
	assert.deepEqual(second.refs("may"), [ref(ID), ref(ID2)]);
	assert.equal(next.sends.length, 2, "both handed to the successor");
	assert.ok(next.sends[0].text.includes(block(ID)) && next.sends[1].text.includes(block(ID2)), "in order");
	assert.deepEqual(child.refs("not_handed").concat(second.refs("not_handed")), []);
});

/// Scenario: An attempt passes the gate, and a reload of a different version replaces the receiver. The reload's successor owns it: a later settle with nothing queued after a run releases it lost, and so does the successor's own /new; either way a revoke is answered already_handed, never not handed.
test("pi_028 a replacement carries the reload's ownership of what passed", async () => {
	for (const end of ["settle", "new"]) {
		const { pi, child } = await started();
		child.hand(ref());
		await settle();
		await pi.gate();
		const { next, finish } = await replacing(pi, child);
		const second = await finish();
		second.register(8, { resume: [ref()] });
		await settle();
		assert.deepEqual(second.sent("possess").at(-1).held, [ref()], `${end}: still held after the replacement`);
		if (end === "new") {
			await next.emit("session_shutdown", { reason: "new" });
		} else {
			await next.emit("agent_start", {});
			await next.emit("agent_settled", {});
		}
		assert.deepEqual(second.sent("possess").at(-1).held, [], `${end}: released lost`);
		second.say({ revoke: [ref()] });
		assert.deepEqual(second.refs("already_handed"), [ref()], `${end}: possibly handed`);
		assert.deepEqual(second.refs("not_handed"), []);
	}
});

/// Scenario: A reload replaces the receiver with one of an incompatible state, and its successor reports Thinking before the outgoing session's delayed shutdown arrives, once with the outgoing token count carried and once from a state with no count to carry. The successor's session number is above the outgoing one's, and either way the outgoing shutdown reports no Idle into the successor's turn.
test("pi_028 a delayed shutdown across an incompatible replacement reports no Idle", async () => {
	for (const carried of [true, false]) {
		const { pi, child } = await started();
		const types: string[] = [];
		const record = async (_bin: string, argv: string[]) => {
			if (argv[0] === "agent-event") types.push(argv[argv.indexOf("--type") + 1]);
			return { code: 0, stdout: "", stderr: "", killed: false };
		};
		pi.pi.exec = record;
		const old = (globalThis as any)[PROCESS];
		const outgoing = old.current.token;
		old.version = OTHER_VERSION;
		old.stateVersion = 0;
		if (!carried) delete old.nextToken;
		const next = instance();
		next.pi.exec = record;
		next.ctx.isIdle = () => false;
		const start = next.emit("session_start", { reason: "reload" });
		await settle();
		child.exit(0);
		await start;
		const current = (globalThis as any)[PROCESS].current.token;
		if (carried) assert.ok(current > outgoing, `the successor's token ${current} is above ${outgoing}`);
		await next.emit("before_agent_start", { prompt: "keep working" });
		await next.emit("agent_start", {});
		await pi.emit("session_shutdown", { reason: "reload" });
		await settle();
		assert.deepEqual(types, ["finished", "prompt", "running"], `carried=${carried}: no Idle after the successor's Thinking`);
	}
});

/// Scenario: The watcher dies while no session is current and is respawned; a later session_start finds it healthy and starts no second one; a watcher that says it runs outside a deck pane is not respawned for the rest of the process.
test("pi_029 the watcher's lifetime is the process's, not a session's", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const { pi, child } = await started();
	await pi.emit("session_shutdown", { reason: "new" });
	child.exit(1);
	t.mock.timers.tick(1000);
	assert.equal(spawned.length, 2, "respawned with no session current");
	const next = instance();
	await next.emit("session_start", { reason: "new" });
	assert.equal(spawned.length, 2, "a healthy watcher is not started again");
	watcher().exit(3);
	t.mock.timers.tick(60000);
	assert.equal(spawned.length, 2, "not in a deck pane: no respawn");
	const later = instance();
	await later.emit("session_start", { reason: "new" });
	assert.equal(spawned.length, 2);
});

/// Scenario: Hand-overs reach the gate late — after a /new swap, after a revoke and after the watcher restarted — and doorbell fetches return late across the same three. Each is consumed, never transformed; the swap's and the rebind's attempts are reported not handed when still held, and the revoke's answer stands alone.
test("pi_032 the session and authorization fence at both hand-over points", async (t) => {
	// (a) a delayed input across a /new swap.
	{
		const { pi, child } = await started();
		child.hand(ref());
		await settle();
		const text = pi.sends[0].text;
		await pi.emit("session_shutdown", { reason: "new" });
		const next = instance();
		await next.emit("session_start", { reason: "new" });
		assert.deepEqual(await pi.emit("input", { text, source: "extension" }), { action: "handled" });
		assert.equal(child.refs("handed").length, 0);
	}
	// (b) a delayed input across a revoke.
	{
		const { pi, child } = await started();
		child.hand(ref());
		await settle();
		child.say({ revoke: [ref()] });
		assert.deepEqual(await pi.gate(), { action: "handled" });
		assert.deepEqual(child.refs("not_handed"), []);
		assert.deepEqual(child.refs("handed"), []);
	}
	// (c) a delayed input across a rebind.
	{
		t.mock.timers.enable({ apis: ["setTimeout"] });
		const { pi, child } = await started();
		child.hand(ref());
		await settle();
		child.exit(1);
		t.mock.timers.tick(1000);
		watcher().register(8, { resume: [ref()] });
		assert.deepEqual(await pi.gate(), { action: "handled" });
		assert.deepEqual(watcher().refs("not_handed"), [ref()]);
		t.mock.timers.reset();
	}
	const line = `dot-agent-deck: message ${ID} waiting, run: dot-agent-deck inbox fetch`;
	/** A doorbell fetch that returns only when `release` is called. */
	const slowFetch = (pi: ReturnType<typeof instance>) => {
		let release!: () => void;
		const gate = new Promise<void>((resolve) => { release = resolve; });
		pi.pi.exec = async (_bin: string, argv: string[]) => {
			if (argv[1] === "fetch") await gate;
			return { code: 0, stdout: JSON.stringify({ messages: argv[1] === "fetch" ? [{ ...ref(), d: 1, text: block() }] : [] }), stderr: "", killed: false };
		};
		return () => release();
	};
	// (d) a delayed fetch across a /new swap.
	{
		const { pi, child } = await started();
		const release = slowFetch(pi);
		const answer = pi.emit("input", { text: line, source: "interactive" });
		await settle();
		await pi.emit("session_shutdown", { reason: "new" });
		await instance().emit("session_start", { reason: "new" });
		release();
		assert.deepEqual(await answer, { action: "handled" });
		assert.deepEqual(child.refs("not_handed"), [ref()]);
		assert.deepEqual(child.refs("handed"), []);
	}
	// (e) a delayed fetch across a revoke of the fetched attempt.
	{
		const { pi, child } = await started();
		let release!: () => void;
		const held = new Promise<void>((resolve) => { release = resolve; });
		pi.pi.exec = async (_bin: string, argv: string[]) => {
			if (argv[1] !== "fetch") return { code: 0, stdout: "", stderr: "", killed: false };
			await held;
			const out = { code: 0, stdout: JSON.stringify({ messages: [{ ...ref(), d: 1, text: block() }] }), stderr: "", killed: false };
			// The deck revokes it as the fetch returns, before the gate's check.
			queueMicrotask(() => child.say({ revoke: [ref()] }));
			return out;
		};
		const answer = pi.emit("input", { text: line, source: "interactive" });
		await settle();
		release();
		const result: any = await answer;
		assert.deepEqual(result, { action: "handled" });
		assert.deepEqual(child.refs("handed"), []);
		assert.deepEqual(child.refs("not_handed"), [], "the revoke's answer stands alone");
	}
	// (f) a delayed fetch across a rebind.
	{
		t.mock.timers.enable({ apis: ["setTimeout"] });
		const { pi, child } = await started();
		const release = slowFetch(pi);
		const answer = pi.emit("input", { text: line, source: "interactive" });
		await settle();
		child.exit(1);
		t.mock.timers.tick(1000);
		watcher().register(8);
		release();
		assert.deepEqual(await answer, { action: "handled" });
		assert.deepEqual(watcher().refs("handed"), []);
		assert.deepEqual(watcher().refs("not_handed"), [ref()]);
	}
});

/**
 * A hand-over of a busy Pi passes the gate while an earlier report holds the
 * process's report queue, then `during` runs, then the queue is let go.
 */
async function behindAHeldReport(t: any, during: (pi: ReturnType<typeof instance>, child: Watcher) => Promise<void>) {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	const { pi, child } = await started();
	const types: string[] = [];
	let release!: () => void;
	const held = new Promise<void>((resolve) => { release = resolve; });
	pi.pi.exec = async (_bin: string, argv: string[]) => {
		if (argv[0] === "agent-event") {
			types.push(argv[argv.indexOf("--type") + 1]);
			await held;
		}
		return { code: 0, stdout: "", stderr: "", killed: false };
	};
	child.hand(ref());
	await settle();
	void pi.emit("agent_start", {});
	await settle();
	let answered: any = "waiting";
	const answer = pi.emit("input", { text: pi.sends[0].text, source: "extension", streamingBehavior: "followUp" }).then((a) => { answered = a; });
	await settle();
	assert.equal(answered?.action, "transform", "the transform returns while the report queue is held");
	await during(pi, child);
	release();
	await answer;
	await settle();
	assert.equal(answered.action, "transform");
	assert.deepEqual(types.slice(0, 2), ["running", "prompt"], "the busy hand-over still reports, after the report ahead of it");
	return { pi, child };
}

/// Scenario: A busy hand-over passes the gate while an earlier status report holds the report queue, and the session is replaced by /new before the queue moves. The transform has already gone back to Pi, so the swap releases the attempt lost and a revoke is answered already_handed, never not handed.
test("pi_032 g a swap while a report holds the queue", async (t) => {
	const { child } = await behindAHeldReport(t, async (pi) => {
		void pi.emit("session_shutdown", { reason: "new" });
		void instance().emit("session_start", { reason: "new" });
		await settle();
	});
	assert.deepEqual(child.sent("possess").at(-1).held, [], "released lost");
	child.say({ revoke: [ref()] });
	assert.deepEqual(child.refs("already_handed"), [ref()]);
	assert.deepEqual(child.refs("not_handed"), []);
});

/// Scenario: A busy hand-over passes the gate while an earlier status report holds the report queue, and the deck revokes the attempt before the queue moves. The transform has already gone back to Pi, so the revoke is answered already_handed.
test("pi_032 h a revoke while a report holds the queue", async (t) => {
	const { child } = await behindAHeldReport(t, async (_pi, child) => {
		child.say({ revoke: [ref()] });
	});
	assert.deepEqual(child.refs("already_handed"), [ref()]);
	assert.deepEqual(child.refs("not_handed"), []);
});

/// Scenario: A busy hand-over passes the gate while an earlier status report holds the report queue, and the watcher restarts and registers again before the queue moves. The new watcher is told the attempt was handed and hears it retained, never not handed.
test("pi_032 i a rebind while a report holds the queue", async (t) => {
	await behindAHeldReport(t, async (_pi, child) => {
		child.exit(1);
		t.mock.timers.tick(1000);
		watcher().register(8, { resume: [ref()] });
		await settle();
	});
	assert.deepEqual(watcher().sent("resume_answers").flat().map((r: any) => r.answer), ["retained"]);
	assert.deepEqual(watcher().refs("handed"), [ref()]);
	assert.deepEqual(watcher().refs("not_handed"), []);
});

/// Scenario: A busy hand-over passes the gate while an earlier status report holds the report queue, and its approval ages past its validity before the queue moves. The hand-over was decided under a fresh approval, so the attempt stays handed and its later start commits it.
test("pi_032 j an approval expiring while a report holds the queue", async (t) => {
	const { pi, child } = await behindAHeldReport(t, async () => {
		const now = performance.now();
		t.mock.method(performance, "now", () => now + 60000);
	});
	await pi.start(ID);
	assert.deepEqual(child.refs("committed"), [ref()]);
	assert.deepEqual(child.refs("not_handed"), []);
});

/// Scenario: Pi runs the gate inside sendUserMessage, as it does for an idle agent, and a callback throws synchronously. The inline gate passes the hand-over recorded before the call, and a throwing sendUserMessage is reported not handed without crashing Pi.
test("pi_inbox_020 the gate inside sendUserMessage, and a throwing call", async () => {
	const { pi, child } = await started();
	pi.inline(true);
	child.hand(ref());
	await settle();
	assert.equal((pi.gated[0] as any)?.action, "transform");
	assert.deepEqual(child.refs("handed"), [ref()]);
	pi.inline(false);
	pi.pi.sendUserMessage = () => { throw new Error("This extension ctx is stale"); };
	child.hand(ref(ID2));
	await settle();
	assert.deepEqual(child.refs("not_handed"), [ref(ID2)]);
	assert.doesNotThrow(() => child.stdout.emit("data", Buffer.from("invalid-json\n")));
	const badEvent = Object.defineProperty({}, "message", { get() { throw new Error("stale event"); } });
	await assert.doesNotReject(async () => pi.emit("message_start", badEvent));
});

/// Scenario: Three messages arrive while Pi is busy and commit one by one at turn boundaries. Each is a follow-up in inbox order and each commit is its own.
test("pi_inbox_003 busy messages keep order and commit each on its start", async () => {
	const { pi, child } = await started();
	pi.ctx.isIdle = () => false;
	const ids = [ID, ID2, "m-k3xzq2ab-cccccccc54b742c9"];
	child.hand(...ids.map((id) => ref(id)));
	await settle(12);
	assert.equal(pi.sends.length, 3);
	pi.sends.forEach((send, i) => {
		assert.ok(send.text.includes(block(ids[i])), "inbox order");
		assert.deepEqual(send.options, { deliverAs: "followUp" });
	});
	for (let i = 0; i < 3; i++) await pi.gate(i);
	for (const id of ids) await pi.start(id);
	assert.deepEqual(child.sent("committed"), ids.map((id) => [ref(id)]));
});

/// Scenario: With DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS set, a watcher that exits stays down exactly that long each time.
test("pi_inbox_008 the backoff override fixes every respawn delay", async (t) => {
	t.mock.timers.enable({ apis: ["setTimeout"] });
	process.env.DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS = "250";
	try {
		await started();
		for (let n = 2; n <= 4; n++) {
			watcher().exit(1);
			t.mock.timers.tick(249);
			assert.equal(spawned.length, n - 1);
			t.mock.timers.tick(1);
			assert.equal(spawned.length, n);
		}
	} finally {
		delete process.env.DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS;
	}
});
