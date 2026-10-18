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
