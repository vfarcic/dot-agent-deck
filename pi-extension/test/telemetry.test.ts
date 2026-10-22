/**
 * Unit tests for the Pi extension's own telemetry export (PRD #1741):
 * inert unless the deck switches it on, metadata only, bounded, and never
 * able to fail Pi. Run: `npm test` (inside pi-extension/).
 */

import assert from "node:assert/strict";
import { createServer, type IncomingHttpHeaders, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { describe, test } from "node:test";
import {
	createPiTelemetry,
	encodeLogs,
	MAX_PENDING,
	parsePairs,
	PiTelemetry,
	SHUTDOWN_TIMEOUT_MS,
	TELEMETRY_ENV,
	type FetchLike,
	telemetryConfig,
	usageRecord,
} from "../src/telemetry.ts";

const ON = {
	[TELEMETRY_ENV]: "1",
	OTEL_EXPORTER_OTLP_ENDPOINT: "http://collector:4318/",
	OTEL_EXPORTER_OTLP_HEADERS: "x-api-key=abc%20def",
	OTEL_RESOURCE_ATTRIBUTES: "deck.run.id=run-1,service.name=user,team=a%2Cb",
};

/** Pi events with a sentinel in every content field Pi hands an extension. */
const LOADED: Array<[string, unknown]> = [
	["session_start", { reason: "SENTINEL-REASON" }],
	["input", { text: "SENTINEL-PROMPT" }],
	["before_agent_start", { prompt: "SENTINEL-PROMPT" }],
	["agent_start", {}],
	["tool_execution_start", { toolName: "bash", args: { command: "SENTINEL-ARGS" } }],
	[
		"tool_execution_end",
		{ toolName: "bash", args: { command: "SENTINEL-ARGS" }, result: { output: "SENTINEL-OUTPUT" }, isError: false, durationMs: 12 },
	],
	[
		"message_end",
		{
			message: {
				role: "assistant",
				model: "claude-haiku-4-5",
				provider: "anthropic",
				content: [{ type: "text", text: "SENTINEL-RESPONSE" }],
				usage: { input: 514, output: 4, cacheRead: 2, cacheWrite: 0, cost: { total: 0.000534 } },
			},
		},
	],
	["message_end", { message: { role: "user", content: "SENTINEL-PROMPT" } }],
	["agent_end", { messages: [{ role: "assistant", content: "SENTINEL-RESPONSE" }] }],
	["session_shutdown", { reason: "SENTINEL-REASON" }],
];

function capture(): { fetch: FetchLike; bodies: string[]; calls: Array<{ url: string; headers: Record<string, string> }> } {
	const bodies: string[] = [];
	const calls: Array<{ url: string; headers: Record<string, string> }> = [];
	return {
		bodies,
		calls,
		fetch: async (url, init) => {
			calls.push({ url, headers: init.headers });
			bodies.push(init.body);
			return {};
		},
	};
}

/** A loopback HTTP server that records each request and answers with `respond`. */
async function listen(
	respond: (res: import("node:http").ServerResponse) => void,
): Promise<{ server: Server; url: string; requests: IncomingHttpHeaders[] }> {
	const requests: IncomingHttpHeaders[] = [];
	const server = createServer((req, res) => {
		requests.push(req.headers);
		req.resume();
		req.on("end", () => respond(res));
	});
	await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
	const { port } = server.address() as AddressInfo;
	return { server, url: `http://127.0.0.1:${port}`, requests };
}

/** A fetch whose requests stay out until released, or until their signal aborts when `honourAbort`. */
function gated(honourAbort: boolean): { fetch: FetchLike; signals: AbortSignal[]; release: () => void } {
	const signals: AbortSignal[] = [];
	const waiting: Array<() => void> = [];
	return {
		signals,
		release: () => {
			for (const done of waiting.splice(0)) {
				done();
			}
		},
		fetch: (_url, init) => {
			signals.push(init.signal);
			return new Promise((resolve, reject) => {
				waiting.push(() => resolve({}));
				if (honourAbort) {
					init.signal.addEventListener("abort", () => reject(init.signal.reason), { once: true });
				}
			});
		},
	};
}

describe("pi telemetry", () => {
	test("inert unless the deck switches it on", () => {
		const { [TELEMETRY_ENV]: _, ...withoutFlag } = ON;
		assert.equal(telemetryConfig(withoutFlag), undefined);
		assert.equal(createPiTelemetry(withoutFlag, capture().fetch), undefined);
		assert.equal(telemetryConfig({ ...ON, [TELEMETRY_ENV]: "0" }), undefined);
		assert.equal(telemetryConfig({ [TELEMETRY_ENV]: "1" }), undefined, "no endpoint");
		assert.equal(telemetryConfig({ [TELEMETRY_ENV]: "1", OTEL_EXPORTER_OTLP_ENDPOINT: "file:///x" }), undefined);
	});

	test("endpoint, headers and resource come from the standard variables", () => {
		const config = telemetryConfig(ON);
		assert.ok(config);
		assert.equal(config.endpoint, "http://collector:4318/v1/logs");
		assert.deepEqual(config.headers, { "x-api-key": "abc def", "content-type": "application/json" });
		assert.deepEqual(config.resource, [
			["deck.run.id", "run-1"],
			["team", "a,b"],
			["service.name", "pi"],
		]);
		const own = telemetryConfig({
			...ON,
			OTEL_EXPORTER_OTLP_LOGS_ENDPOINT: "https://logs/v1/logs",
			OTEL_EXPORTER_OTLP_LOGS_HEADERS: "k=v",
		});
		assert.equal(own?.endpoint, "https://logs/v1/logs");
		assert.deepEqual(own?.headers, { k: "v", "content-type": "application/json" });
		assert.deepEqual(parsePairs("a=1,=x,b,a=2"), [["a", "2"]]);
	});

	test("records carry metadata only", async () => {
		const sink = capture();
		const config = telemetryConfig(ON);
		assert.ok(config);
		const telemetry = new PiTelemetry(config, sink.fetch, () => 1_700_000_000_000, 0);
		for (const [name, payload] of LOADED) {
			telemetry.observe(name, payload);
		}
		await telemetry.shutdown();
		assert.equal(sink.bodies.length, 1);
		const body = sink.bodies[0];
		for (const sentinel of ["SENTINEL-PROMPT", "SENTINEL-ARGS", "SENTINEL-OUTPUT", "SENTINEL-RESPONSE", "SENTINEL-REASON"]) {
			assert.ok(!body.includes(sentinel), `${sentinel} leaked: ${body}`);
		}
		const parsed = JSON.parse(body);
		const records = parsed.resourceLogs[0].scopeLogs[0].logRecords;
		assert.deepEqual(
			records.map((r: { body: { stringValue: string } }) => r.body.stringValue),
			["pi.session_start", "pi.agent_start", "pi.tool_result", "pi.model_usage", "pi.agent_end", "pi.session_end"],
		);
		const usage = records[3].attributes;
		const value = (key: string) => usage.find((a: { key: string }) => a.key === key)?.value;
		assert.deepEqual(value("pi.model"), { stringValue: "claude-haiku-4-5" });
		assert.deepEqual(value("pi.tokens.input"), { intValue: "514" });
		assert.deepEqual(value("pi.cost.total"), { doubleValue: 0.000534 });
		assert.deepEqual(records[0].timeUnixNano, "1700000000000000000");
		const resource = parsed.resourceLogs[0].resource.attributes;
		assert.ok(resource.some((a: { key: string; value: { stringValue: string } }) => a.key === "deck.run.id" && a.value.stringValue === "run-1"));
		assert.deepEqual(sink.calls[0], {
			url: "http://collector:4318/v1/logs",
			headers: { "x-api-key": "abc def", "content-type": "application/json" },
		});
	});

	test("bounded, batched and silent on failure", async () => {
		const config = telemetryConfig(ON);
		assert.ok(config);
		let calls = 0;
		const failing: FetchLike = async () => {
			calls += 1;
			throw new Error("backend down");
		};
		const telemetry = new PiTelemetry(config, failing, Date.now, 0);
		for (let i = 0; i < MAX_PENDING + 5; i++) {
			telemetry.observe("agent_end", {});
		}
		assert.equal(telemetry.dropped, 5);
		await telemetry.flush();
		await telemetry.flush();
		assert.equal(calls, 1, "an empty batch sends nothing");
		telemetry.observe("message_end", null);
		telemetry.observe("unknown_event", { anything: 1 });
		await telemetry.flush();
		assert.equal(calls, 1, "events that yield no record send nothing");
	});

	test("unknown events and non-assistant messages yield nothing", () => {
		assert.equal(usageRecord("input", { text: "x" }), undefined);
		assert.equal(usageRecord("message_end", { message: { role: "user" } }), undefined);
		assert.deepEqual(usageRecord("tool_execution_end", { toolName: "read", isError: true }), {
			name: "pi.tool_result",
			attributes: { "pi.tool.name": "read", "pi.tool.success": false },
		});
		assert.match(encodeLogs({ endpoint: "x", headers: {}, resource: [] }, []), /"logRecords":\[\]/);
	});

	test("a redirect is not followed, so the headers reach no other origin", async () => {
		const other = await listen((res) => res.end());
		const first = await listen((res) => {
			res.writeHead(307, { location: `${other.url}/v1/logs` });
			res.end();
		});
		try {
			const telemetry = createPiTelemetry({
				[TELEMETRY_ENV]: "1",
				OTEL_EXPORTER_OTLP_ENDPOINT: first.url,
				OTEL_EXPORTER_OTLP_HEADERS: "x-honeycomb-team=synthetic-credential-1741",
			});
			assert.ok(telemetry);
			telemetry.observe("agent_end", {});
			await telemetry.flush();
			await telemetry.shutdown();
			assert.equal(first.requests.length, 1, "the configured endpoint was asked");
			assert.equal(first.requests[0]["x-honeycomb-team"], "synthetic-credential-1741");
			assert.deepEqual(other.requests, [], "the redirect target received nothing");
		} finally {
			first.server.close();
			other.server.close();
		}
		const sink = capture();
		let redirect: string | undefined;
		const telemetry = new PiTelemetry(telemetryConfig(ON)!, async (url, init) => {
			redirect = init.redirect;
			return sink.fetch(url, init);
		}, Date.now, 0);
		telemetry.observe("agent_end", {});
		await telemetry.flush();
		assert.equal(redirect, "error");
	});

	test("flushes while a request is out join it and send the rest after it", async () => {
		const config = telemetryConfig(ON);
		assert.ok(config);
		const gate = gated(false);
		const telemetry = new PiTelemetry(config, gate.fetch, Date.now, 0);
		telemetry.observe("agent_start", {});
		const first = telemetry.flush();
		telemetry.observe("agent_end", {});
		const joined = Array.from({ length: 100 }, () => telemetry.flush());
		assert.ok(joined.every((p) => p === first), "one worker, not a chain of 100");
		await new Promise((resolve) => setImmediate(resolve));
		assert.equal(gate.signals.length, 1, "one request out at a time");
		gate.release();
		await new Promise((resolve) => setImmediate(resolve));
		assert.equal(gate.signals.length, 2, "what arrived meanwhile goes next");
		gate.release();
		await first;
	});

	for (const honourAbort of [true, false]) {
		test(`shutdown waits at most its deadline for a request already out (fetch ${honourAbort ? "honours" : "ignores"} abort)`, { timeout: 10_000 }, async () => {
			const config = telemetryConfig(ON);
			assert.ok(config);
			const gate = gated(honourAbort);
			const telemetry = new PiTelemetry(config, gate.fetch, Date.now, 0);
			telemetry.observe("agent_start", {});
			void telemetry.flush();
			telemetry.observe("session_shutdown", {});
			const started = Date.now();
			await telemetry.shutdown();
			const waited = Date.now() - started;
			assert.ok(waited >= SHUTDOWN_TIMEOUT_MS - 50, `returned early: ${waited} ms`);
			assert.ok(waited < SHUTDOWN_TIMEOUT_MS + 500, `waited past the deadline: ${waited} ms`);
			assert.ok(gate.signals[0].aborted, "the outstanding request was cancelled");
			gate.release();
		});
	}
});
