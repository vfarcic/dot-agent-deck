/**
 * Unit tests for the Pi extension's usage-file lines (PRD #1858): one line
 * per assistant reply and per turn start, named fields only, in the format
 * the deck reads, appended only to a file that is safe to append to.
 * `usage-file-cases.json` is shared with the deck's reader
 * (`src/telemetry/accounting/usage_file.rs`), which reads each case's line
 * and checks what it makes of it, so a producer change that drifts from the
 * reader fails one side or the other.
 * Run: `npm test` (inside pi-extension/).
 */

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, test } from "node:test";
import {
	USAGE_FILE_ENV,
	appendUsageLine,
	usageFileLine,
	usageFilePath,
	usageTurnStartLine,
} from "../src/usage-file.ts";

interface Case {
	name: string;
	usage?: Record<string, unknown>;
	stopReason: string;
	line: Record<string, unknown>;
}

const cases: Case[] = JSON.parse(readFileSync(new URL("./usage-file-cases.json", import.meta.url), "utf8"));

const reply = (overrides: Record<string, unknown> = {}) => ({
	message: {
		role: "assistant",
		model: "gpt-5.4-mini",
		provider: "openai",
		stopReason: "stop",
		content: [{ type: "text", text: "SECRET_REPLY" }],
		errorMessage: "SECRET_ERROR",
		usage: { input: 100, output: 20, cacheRead: 50, cacheWrite: 0, cost: { total: 0.0042 } },
		...overrides,
	},
});

describe("usage-file lines", () => {
	for (const c of cases) {
		test(`${c.name}: the line the deck's reader is tested against`, () => {
			const line = usageFileLine(reply({ usage: c.usage, stopReason: c.stopReason }));
			assert.deepEqual(JSON.parse(line ?? ""), c.line);
		});
	}

	test("no content, error message or free-text name reaches the line", () => {
		const line = usageFileLine(reply({ model: "a model with spaces", stopReason: "SECRET stop" })) ?? "";
		for (const secret of ["SECRET", "spaces"]) {
			assert.ok(!line.includes(secret), line);
		}
		assert.equal(JSON.parse(line).model, undefined);
	});

	test("a message that is not an assistant reply has no line", () => {
		assert.equal(usageFileLine({ message: { role: "user", content: "hi" } }), undefined);
		assert.equal(usageFileLine(undefined), undefined);
	});

	test("a turn's start is a line of its own, with no usage", () => {
		assert.deepEqual(JSON.parse(usageTurnStartLine()), { v: 1, type: "turn_start", agent: "pi" });
	});

	test("the file is honoured only as an absolute path", () => {
		assert.equal(usageFilePath({ [USAGE_FILE_ENV]: "/state/run.usage.jsonl" }, "linux"), "/state/run.usage.jsonl");
		assert.equal(usageFilePath({ [USAGE_FILE_ENV]: "run.usage.jsonl" }, "linux"), undefined);
		assert.equal(usageFilePath({ [USAGE_FILE_ENV]: "" }, "linux"), undefined);
		assert.equal(usageFilePath({}, "linux"), undefined);
	});
});

describe("appending to the usage file", { skip: process.platform === "win32" }, () => {
	const scratch = () => mkdtempSync(join(tmpdir(), "pi-usage-"));

	test("a private regular file the deck made is appended to", () => {
		const file = join(scratch(), "run.usage.jsonl");
		writeFileSync(file, "", { mode: 0o600 });
		assert.equal(appendUsageLine(file, "one"), "appended");
		assert.equal(appendUsageLine(file, "two"), "appended");
		assert.equal(readFileSync(file, "utf8"), "one\ntwo\n");
	});

	test("a missing file is not created", () => {
		const file = join(scratch(), "run.usage.jsonl");
		assert.equal(appendUsageLine(file, "one"), "unsafe");
		assert.throws(() => readFileSync(file));
	});

	test("a symlink is not followed", () => {
		const dir = scratch();
		const target = join(dir, "elsewhere");
		writeFileSync(target, "", { mode: 0o600 });
		const file = join(dir, "run.usage.jsonl");
		symlinkSync(target, file);
		assert.equal(appendUsageLine(file, "one"), "unsafe");
		assert.equal(readFileSync(target, "utf8"), "");
	});

	test("a file other users can read or write is not appended to", () => {
		const file = join(scratch(), "run.usage.jsonl");
		writeFileSync(file, "", { mode: 0o644 });
		assert.equal(appendUsageLine(file, "one"), "unsafe");
		assert.equal(readFileSync(file, "utf8"), "");
	});

	test("a FIFO neither blocks nor is written to", () => {
		const file = join(scratch(), "run.usage.jsonl");
		execFileSync("mkfifo", ["-m", "600", file]);
		const started = Date.now();
		assert.equal(appendUsageLine(file, "one"), "unsafe");
		assert.ok(Date.now() - started < 1000);
	});
});
