/**
 * PRD #1542, catalog `question/detect/008`: the deck's wrapper around another
 * Pi extension's `ctx.ui` dialogs.
 *
 * Scenario: a stand-in `ctx.ui` whose `select` / `confirm` / `input` behave like
 * Pi's — they resolve when "the keyboard" answers and resolve `undefined` when
 * their AbortSignal fires — is wrapped, and a stand-in `exec` plays
 * `dot-agent-deck await-answer`. A deck answer must abort the original dialog
 * and resolve the asking extension with the deck's value; a keyboard answer must
 * abort the `await-answer` child and resolve with the keyboard's value; a Pi
 * whose `ctx.ui` is missing or read-only must be left alone, never crash.
 *
 * Run: `node --test test/*.test.ts` (Node strips the types) or `npm test`.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";
import {
	buildAwaitAnswerArgv,
	installQuestionWrappers,
	parseAwaitAnswer,
	type PiDialog,
	type QuestionDeps,
} from "../src/orchestrator.ts";

/** A dialog that the test answers by hand, or that its signal dismisses. */
function fakeDialog() {
	const calls: { args: unknown[]; signal?: AbortSignal; answer: (v: unknown) => void }[] = [];
	const method = (...args: unknown[]) =>
		new Promise((resolve) => {
			const opts = args[args.length - 1] as { signal?: AbortSignal } | undefined;
			const signal = opts?.signal;
			signal?.addEventListener("abort", () => resolve(undefined), { once: true });
			calls.push({ args, signal, answer: resolve });
		});
	return { method, calls };
}

/** An `exec` that resolves when the test says the deck answered. */
function fakeDeck() {
	const runs: { argv: string[]; signal: AbortSignal; reply: (stdout: string) => void }[] = [];
	const deps: QuestionDeps = {
		exec: (argv, signal) =>
			new Promise((resolve, reject) => {
				signal.addEventListener("abort", () => reject(new Error("aborted")), { once: true });
				runs.push({ argv, signal, reply: (stdout) => resolve({ code: 0, stdout }) });
			}),
		mintId: () => "q-0123456789abcdef0123456789abcdef",
	};
	return { deps, runs };
}

const tick = () => new Promise((resolve) => setImmediate(resolve));

describe("question/detect/008 — Pi ctx.ui wrapper", () => {
	test("builds the await-answer argv with the dialog as JSON", () => {
		const dialog: PiDialog = { id: "q-1", kind: "select", title: "Pick", options: ["Red", "Blue"] };
		const argv = buildAwaitAnswerArgv(dialog);
		assert.deepEqual(argv.slice(0, 4), ["await-answer", "--agent", "pi", "--question"]);
		assert.deepEqual(JSON.parse(argv[4]), dialog);
	});

	test("parses only a value of the dialog's own type", () => {
		assert.deepEqual(parseAwaitAnswer("select", '{"value":"Blue"}\n'), { value: "Blue" });
		assert.deepEqual(parseAwaitAnswer("confirm", '{"value":true}'), { value: true });
		assert.equal(parseAwaitAnswer("confirm", '{"value":"yes"}'), undefined);
		assert.equal(parseAwaitAnswer("select", ""), undefined);
		assert.equal(parseAwaitAnswer("select", "not json"), undefined);
		assert.equal(parseAwaitAnswer("input", '{"other":1}'), undefined);
	});

	test("a deck answer to select aborts the dialog and resolves the caller", async () => {
		const select = fakeDialog();
		const ui: Record<string, unknown> = { select: select.method };
		const deck = fakeDeck();
		assert.equal(installQuestionWrappers(ui, deck.deps), 1);
		const pending = (ui.select as (t: string, o: string[]) => Promise<unknown>)("Pick a colour", ["Red", "Green", "Blue"]);
		await tick();
		assert.equal(select.calls.length, 1, "the original dialog is shown");
		assert.equal(deck.runs.length, 1, "await-answer is started");
		const dialog = JSON.parse(deck.runs[0].argv[4]);
		assert.equal(dialog.kind, "select");
		assert.deepEqual(dialog.options, ["Red", "Green", "Blue"]);
		deck.runs[0].reply('{"value":"Blue"}\n');
		assert.equal(await pending, "Blue");
		assert.equal(select.calls[0].signal?.aborted, true, "the deck's answer dismissed the dialog");
	});

	test("a keyboard answer to confirm aborts await-answer and wins", async () => {
		const confirm = fakeDialog();
		const ui: Record<string, unknown> = { confirm: confirm.method };
		const deck = fakeDeck();
		installQuestionWrappers(ui, deck.deps);
		const pending = (ui.confirm as (t: string, m: string) => Promise<unknown>)("Allow rm?", "Run rm -rf build?");
		await tick();
		const dialog = JSON.parse(deck.runs[0].argv[4]);
		assert.equal(dialog.kind, "confirm");
		assert.equal(dialog.message, "Run rm -rf build?");
		confirm.calls[0].answer(false);
		assert.equal(await pending, false);
		assert.equal(deck.runs[0].signal.aborted, true, "the keyboard's answer stopped the deck's wait");
	});

	test("an input answered by the deck resolves with its text", async () => {
		const input = fakeDialog();
		const ui: Record<string, unknown> = { input: input.method };
		const deck = fakeDeck();
		installQuestionWrappers(ui, deck.deps);
		const pending = (ui.input as (t: string, p: string) => Promise<unknown>)("Name?", "your name");
		await tick();
		const dialog = JSON.parse(deck.runs[0].argv[4]);
		assert.equal(dialog.kind, "input");
		assert.equal(dialog.placeholder, "your name");
		deck.runs[0].reply('{"value":"Bob"}');
		assert.equal(await pending, "Bob");
	});

	test("a deck that answers nothing leaves the dialog to the keyboard", async () => {
		const select = fakeDialog();
		const ui: Record<string, unknown> = { select: select.method };
		const deck = fakeDeck();
		installQuestionWrappers(ui, deck.deps);
		const pending = (ui.select as (t: string, o: string[]) => Promise<unknown>)("Pick", ["A", "B"]);
		await tick();
		deck.runs[0].reply("");
		await tick();
		assert.equal(select.calls[0].signal?.aborted, false, "an empty reply does not dismiss the dialog");
		select.calls[0].answer("A");
		assert.equal(await pending, "A");
	});

	test("wrapping twice leaves one wrapper", () => {
		const select = fakeDialog();
		const ui: Record<string, unknown> = { select: select.method };
		const deck = fakeDeck();
		assert.equal(installQuestionWrappers(ui, deck.deps), 1);
		const first = ui.select;
		assert.equal(installQuestionWrappers(ui, deck.deps), 0);
		assert.equal(ui.select, first);
	});

	test("a missing or read-only ctx.ui is left alone and never throws", () => {
		const deck = fakeDeck();
		assert.equal(installQuestionWrappers(undefined, deck.deps), 0);
		assert.equal(installQuestionWrappers({}, deck.deps), 0);
		const frozen = Object.freeze({ select: fakeDialog().method });
		assert.equal(installQuestionWrappers(frozen, deck.deps), 0);
		const getterOnly = {};
		Object.defineProperty(getterOnly, "confirm", { get: () => fakeDialog().method });
		assert.equal(installQuestionWrappers(getterOnly, deck.deps), 0);
	});
});
