/** Tests the generated inbox receiver module the Pi extension imports (PRD #1666). */
import assert from "node:assert/strict";
import { chmodSync, existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { afterEach, beforeEach, test } from "node:test";
import { inboxBinary, inboxReceiver } from "../src/inbox-receiver.js";

const settle = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

let dir: string;
let saved: Record<string, string | undefined>;
beforeEach(() => {
	dir = mkdtempSync(join(tmpdir(), "dad pi receiver "));
	saved = { PATH: process.env.PATH, BACKOFF: process.env.DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS };
	process.env.DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS = "20";
});
afterEach(() => {
	inboxBinary(() => undefined);
	process.env.PATH = saved.PATH;
	if (saved.BACKOFF === undefined) delete process.env.DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS;
	else process.env.DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS = saved.BACKOFF;
	rmSync(dir, { recursive: true, force: true });
});

// A stand-in deck binary that records that it ran, then exits.
function fakeDeck(name: string, mode: number): { path: string; marker: string } {
	const path = join(dir, name);
	const marker = join(dir, name + ".ran");
	writeFileSync(path, `#!/bin/sh\necho ran > '${marker}'\n`);
	chmodSync(path, mode);
	return { path, marker };
}

// Starts a receiver whose resolver gives `resolved`, lets it run for a while
// and stops it; returns how many spawns it resolved the binary for and
// whether anything was delivered.
async function run(resolved: () => string | undefined) {
	let resolutions = 0;
	inboxBinary(() => {
		resolutions += 1;
		return resolved();
	});
	let delivered = 0;
	let on = true;
	const receiver = inboxReceiver({ agent: "pi", active: () => on, deliver: () => { delivered += 1; } });
	receiver.start();
	await settle(250);
	const rx = receiver.rx();
	on = false;
	receiver.stop();
	await settle(50);
	return { resolutions, delivered, rx };
}

test("pi_031 the receiver runs only an absolute path to an executable file", { skip: process.platform === "win32" }, async (t) => {
	await t.test("a bare name the agent's PATH would find is not run, and the receiver tries again", async () => {
		const deck = fakeDeck("dot-agent-deck", 0o755);
		process.env.PATH = dir + ":" + (saved.PATH ?? "");
		const outcome = await run(() => "dot-agent-deck");
		assert.equal(existsSync(deck.marker), false, "a bare name ran through PATH");
		assert.ok(outcome.resolutions > 1, `no retry after a refused bare name (${outcome.resolutions})`);
		assert.equal(outcome.delivered, 0);
		assert.equal(outcome.rx, undefined);
	});
	await t.test("a path relative to the working directory is not run", async () => {
		const deck = fakeDeck("relative-deck", 0o755);
		const outcome = await run(() => relative(process.cwd(), deck.path));
		assert.equal(existsSync(deck.marker), false, "a relative path ran");
		assert.ok(outcome.resolutions > 1, `no retry after a refused relative path (${outcome.resolutions})`);
		assert.equal(outcome.delivered, 0);
	});
	await t.test("nothing resolved, a missing file and a non-executable file are not run, and each is tried again", async () => {
		const plain = fakeDeck("not-executable", 0o644);
		for (const [what, resolved] of [
			["unset", () => undefined],
			["missing", () => join(dir, "missing-deck")],
			["not executable", () => plain.path],
		] as const) {
			const outcome = await run(resolved);
			assert.ok(outcome.resolutions > 1, `${what}: no retry (${outcome.resolutions})`);
			assert.equal(outcome.delivered, 0, what);
			assert.equal(outcome.rx, undefined, what);
		}
		assert.equal(existsSync(plain.marker), false);
	});
	await t.test("an absolute path to an executable, spaces included, is run", async () => {
		const deck = fakeDeck("dot agent deck", 0o755);
		await run(() => deck.path);
		assert.equal(existsSync(deck.marker), true, "the absolute executable did not run");
	});
});

// A stand-in watcher that prints `lines`, one JSON value per line, then stays
// up until the receiver stops it.
function fakeWatcher(name: string, lines: unknown[]): string {
	const path = join(dir, name);
	const out = join(dir, name + ".out");
	writeFileSync(out, lines.map((line) => JSON.stringify(line) + "\n").join(""));
	writeFileSync(path, `#!/bin/sh\ncat '${out}'\nexec sleep 5\n`);
	chmodSync(path, 0o755);
	return path;
}

const REGISTERED = { registered: { rx: 7, slot: "pi", keepalive_ms: 1000 }, resume: [] };

/// Scenario: The watcher hands out one message missing its text and one whole one. Only the whole one is held and delivered.
test("pi_031 a hand-out missing its text is neither held nor delivered", { skip: process.platform === "win32" }, async () => {
	const watcher = fakeWatcher("watcher", [
		REGISTERED,
		{ messages: [{ id: "m-bad", a: 1, e: 1, d: 1 }, { id: "m-good", a: 1, e: 1, d: 2, text: "hello" }] },
	]);
	inboxBinary(() => watcher);
	const delivered: string[] = [];
	let on = true;
	const receiver = inboxReceiver({ agent: "pi", active: () => on, deliver: (message: { id: string }) => delivered.push(message.id) });
	receiver.start();
	for (let waited = 0; delivered.length === 0 && waited < 3000; waited += 20) await settle(20);
	const held = (receiver.exportState().held ?? []).map((entry: { id: string }) => entry.id);
	on = false;
	receiver.stop();
	assert.deepEqual(delivered, ["m-good"]);
	assert.deepEqual(held, ["m-good"]);
});

/// Scenario: The watcher hands out a message, then revokes it and an attempt the receiver never held. The receiver tells its owner once per revoke, each time after it answered that revoke, so the revoked message is no longer held when it hears.
test("pi_031 each answered revoke is reported to the receiver's owner", { skip: process.platform === "win32" }, async () => {
	const watcher = fakeWatcher("watcher", [
		REGISTERED,
		{ messages: [{ id: "m-held", a: 1, e: 1, d: 1, text: "hello" }] },
		{ revoke: [{ id: "m-held", a: 1, e: 1 }, { id: "m-never", a: 1, e: 1 }] },
	]);
	inboxBinary(() => watcher);
	const heard: string[][] = [];
	let on = true;
	const receiver = inboxReceiver({
		agent: "pi",
		active: () => on,
		deliver: () => {},
		revokeAnswered: () => heard.push((receiver.exportState().held ?? []).map((entry: { id: string }) => entry.id)),
	});
	receiver.start();
	for (let waited = 0; heard.length < 2 && waited < 3000; waited += 20) await settle(20);
	await settle(50);
	on = false;
	receiver.stop();
	assert.deepEqual(heard, [[], []], "once per revoke, after the revoked message is dropped");
});

/// Scenario: The watcher revokes more attempts than the receiver remembers. It keeps the newest ones only, up to its bound.
test("pi_031 the receiver's memory of revoked attempts is bounded", { skip: process.platform === "win32" }, async () => {
	const revoke = Array.from({ length: 4100 }, (_, n) => ({ id: "m-" + n, a: 1, e: 1 }));
	const watcher = fakeWatcher("watcher", [REGISTERED, { revoke }]);
	inboxBinary(() => watcher);
	let on = true;
	const receiver = inboxReceiver({ agent: "pi", active: () => on });
	receiver.start();
	let revoked: string[] = [];
	for (let waited = 0; revoked.length < 4096 && waited < 3000; waited += 20) {
		await settle(20);
		revoked = receiver.exportState().revoked ?? [];
	}
	on = false;
	receiver.stop();
	assert.equal(revoked.length, 4096);
	assert.ok(revoked.includes("m-4099|1|1"), "the newest is kept");
	assert.ok(!revoked.includes("m-0|1|1"), "the oldest is dropped");
});

/// Scenario: Under an approval given before any revoke, the watcher revokes an attempt and then more attempts than the receiver remembers, so the first is forgotten. A late doorbell fetch then brings that attempt back: it is held, but not ready under that approval, while an approval given now still readies a held attempt.
test("pi_031 a forgotten revoke never becomes ready under an approval given before it", { skip: process.platform === "win32" }, async () => {
	const revoke = Array.from({ length: 4097 }, (_, n) => ({ id: "m-" + n, a: 1, e: 1 }));
	const path = join(dir, "watcher");
	writeFileSync(join(dir, "registered"), JSON.stringify(REGISTERED) + "\n");
	writeFileSync(join(dir, "revokes"), JSON.stringify({ revoke }) + "\n");
	writeFileSync(path, `#!/bin/sh\ncat '${join(dir, "registered")}'\nsleep 0.3\ncat '${join(dir, "revokes")}'\nexec sleep 5\n`);
	chmodSync(path, 0o755);
	inboxBinary(() => path);
	let on = true;
	const receiver = inboxReceiver({ agent: "pi", active: () => on });
	receiver.start();
	for (let waited = 0; receiver.rx() === undefined && waited < 3000; waited += 10) await settle(10);
	const before = receiver.approval();
	let revoked: string[] = [];
	for (let waited = 0; !revoked.includes("m-4096|1|1") && waited < 3000; waited += 20) {
		await settle(20);
		revoked = receiver.exportState().revoked ?? [];
	}
	const forgotten = { id: "m-0", a: 1, e: 1 };
	const other = { id: "m-other", a: 1, e: 1 };
	receiver.take([{ ...forgotten, d: 1, text: "late" }, { ...other, d: 2, text: "never revoked" }]);
	const lateReady = receiver.ready(forgotten, before);
	const nowReady = receiver.ready(other, receiver.approval());
	const age = performance.now() - before.at;
	on = false;
	receiver.stop();
	assert.ok(!revoked.includes("m-0|1|1"), "precondition: the first revoke was forgotten");
	assert.ok(age < before.valid, `precondition: the earlier approval is still within its time (${age} ms)`);
	assert.equal(nowReady, true, "precondition: the receiver readies a held attempt under a new approval");
	assert.equal(lateReady, false, "a forgotten revoke became ready under an approval given before it");
});

/// Scenario: A receiver starts from another receiver's state holding more revoked and called attempts than it remembers. It keeps the newest of each, up to its bound, and counts the dropped called ones as possibly handed.
test("pi_031 a transferred memory of revoked and called attempts is bounded", () => {
	const keys = Array.from({ length: 5000 }, (_, n) => "m-" + n + "|1|1");
	const receiver = inboxReceiver({ agent: "pi", active: () => false, state: { held: [], revoked: keys, called: keys, reports: [] } });
	const state = receiver.exportState();
	const revoked = state.revoked ?? [];
	assert.equal(revoked.length, 4096);
	assert.ok(revoked.includes("m-4999|1|1") && !revoked.includes("m-0|1|1"), "the newest revoked are kept");
	assert.equal((state.called ?? []).length, 4096);
	assert.equal(state.forgot, true, "a dropped called attempt counts as possibly handed");
});

/// Scenario: The resolved deck binary is a script whose interpreter is missing, so Node fails the spawn after it returned, with an error and no exit. The receiver tries again on its backoff.
test("pi_031 a spawn Node fails after returning is tried again", { skip: process.platform === "win32" }, async () => {
	const path = join(dir, "no-interpreter");
	writeFileSync(path, "#!/nonexistent/dot-agent-deck-interpreter\n");
	chmodSync(path, 0o755);
	const outcome = await run(() => path);
	assert.ok(outcome.resolutions > 1, `no retry after a spawn that failed late (${outcome.resolutions})`);
	assert.equal(outcome.rx, undefined);
});
