/**
 * Unit tests for the Pi extension's usage-file lines (PRD #1858): one line
 * per assistant reply, named fields only, in the format the deck reads.
 * Run: `npm test` (inside pi-extension/).
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";
import { USAGE_FILE_ENV, usageFileLine, usageFilePath } from "../src/orchestrator.ts";

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
	test("an assistant reply is one line of counts, cost and stop reason", () => {
		assert.deepEqual(JSON.parse(usageFileLine(reply()) ?? ""), {
			v: 1,
			type: "reply",
			agent: "pi",
			model: "gpt-5.4-mini",
			input: 100,
			output: 20,
			cache_read: 50,
			cache_write: 0,
			cost_usd: 0.0042,
			stop: "stop",
		});
	});

	test("no content, error message or free-text name reaches the line", () => {
		const line = usageFileLine(reply({ model: "a model with spaces", stopReason: "SECRET stop" })) ?? "";
		for (const secret of ["SECRET", "spaces"]) {
			assert.ok(!line.includes(secret), line);
		}
		assert.equal(JSON.parse(line).model, undefined);
	});

	test("a reply without usage says the provider reported none", () => {
		const parsed = JSON.parse(usageFileLine(reply({ usage: undefined, stopReason: "error" })) ?? "");
		assert.equal(parsed.reported, false);
		assert.equal(parsed.cost_usd, undefined);
		assert.equal(parsed.stop, "error");
	});

	test("a message that is not an assistant reply has no line", () => {
		assert.equal(usageFileLine({ message: { role: "user", content: "hi" } }), undefined);
		assert.equal(usageFileLine(undefined), undefined);
	});

	test("the file is honoured only as an absolute path", () => {
		assert.equal(usageFilePath({ [USAGE_FILE_ENV]: "/state/run.usage.jsonl" }, "linux"), "/state/run.usage.jsonl");
		assert.equal(usageFilePath({ [USAGE_FILE_ENV]: "run.usage.jsonl" }, "linux"), undefined);
		assert.equal(usageFilePath({}, "linux"), undefined);
	});
});
