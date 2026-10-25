/**
 * PRD #1741: Pi's own telemetry, exported by the deck's extension.
 *
 * Pi has no OTLP exporter of its own, so when the deck exports telemetry it
 * tells this extension to (`DOT_AGENT_DECK_PI_TELEMETRY=1`, set only then and
 * only when there is somewhere for logs to go). Without that variable nothing
 * here runs: no timer, no request.
 *
 * What is sent is metadata only, as OTLP/JSON log records: each assistant
 * message's model, provider, token counts and cost; each tool call's name,
 * whether it succeeded and how long it took; and the session's and the agent
 * loop's start and end. Never a prompt, a response, a tool's arguments or its
 * output — the mapping below ({@link usageRecord}) reads only the fields it
 * sends. The resource is the agent's `OTEL_RESOURCE_ATTRIBUTES`, which carries
 * the deck's run identity, with `service.name=pi`; headers come from the
 * standard `OTEL_EXPORTER_OTLP_*HEADERS` variables.
 *
 * Bounded and harmless to Pi: at most {@link MAX_PENDING} records wait, a
 * batch goes every {@link BATCH_INTERVAL_MS} and at the end of each agent
 * loop, one request is out at a time and gives up after
 * {@link EXPORT_TIMEOUT_MS}, and a failure is dropped silently. The session's
 * shutdown waits at most {@link SHUTDOWN_TIMEOUT_MS} in all — for a request
 * already out and for the last batch — and cancels whatever is still out then.
 *
 * A redirect is an error, not followed: the headers carry the backend's
 * credentials, and they go to the configured endpoint only.
 *
 * Import-free apart from Node's globals, so it is testable without Pi.
 */

/** Set by the deck when this extension should export. */
export const TELEMETRY_ENV = "DOT_AGENT_DECK_PI_TELEMETRY";

/** Records held for the next batch; past it new ones are dropped. */
export const MAX_PENDING = 512;

/** How often a batch is sent while the session runs. */
export const BATCH_INTERVAL_MS = 5000;

/** How long one export may take. */
export const EXPORT_TIMEOUT_MS = 5000;

/** How long the session's shutdown may wait for exports, in all. */
export const SHUTDOWN_TIMEOUT_MS = 2000;

/** The longest string attribute sent (a model or tool name). */
const MAX_NAME_CHARS = 128;

export type Env = Record<string, string | undefined>;

export interface TelemetryConfig {
	endpoint: string;
	headers: Record<string, string>;
	resource: Array<[string, string]>;
}

type AttributeValue = string | number | boolean;

/** One record before encoding: its event name and its attributes. */
export interface UsageRecord {
	name: string;
	attributes: Record<string, AttributeValue>;
}

const nonEmpty = (value: string | undefined): string | undefined => {
	const trimmed = value?.trim();
	return trimmed === undefined || trimmed === "" ? undefined : trimmed;
};

/** `k=v,k2=v2` with percent-encoded values, as the OTel variables spell it. */
export function parsePairs(raw: string | undefined): Array<[string, string]> {
	const out: Array<[string, string]> = [];
	for (const part of (raw ?? "").split(",")) {
		const at = part.indexOf("=");
		if (at <= 0) {
			continue;
		}
		const key = part.slice(0, at).trim();
		let value = part.slice(at + 1).trim();
		try {
			value = decodeURIComponent(value);
		} catch {
			// Kept as written.
		}
		if (key !== "") {
			const existing = out.findIndex(([k]) => k === key);
			if (existing >= 0) {
				out[existing] = [key, value];
			} else {
				out.push([key, value]);
			}
		}
	}
	return out;
}

/**
 * Where and how to export, or `undefined` when this extension should not: the
 * deck has not switched it on, `OTEL_SDK_DISABLED` is `true` (any case),
 * `OTEL_LOGS_EXPORTER` is set and names no `otlp` exporter, or no http(s)
 * endpoint is configured. The opt-outs are read here as well as by the deck,
 * since a wrapper can change the environment after the deck's decision.
 */
export function telemetryConfig(env: Env): TelemetryConfig | undefined {
	if (env[TELEMETRY_ENV] !== "1") {
		return undefined;
	}
	if (nonEmpty(env.OTEL_SDK_DISABLED)?.toLowerCase() === "true") {
		return undefined;
	}
	const exporters = nonEmpty(env.OTEL_LOGS_EXPORTER);
	if (exporters !== undefined && !exporters.split(",").some((e) => e.trim().toLowerCase() === "otlp")) {
		return undefined;
	}
	const general = nonEmpty(env.OTEL_EXPORTER_OTLP_ENDPOINT);
	const endpoint =
		nonEmpty(env.OTEL_EXPORTER_OTLP_LOGS_ENDPOINT) ??
		(general === undefined ? undefined : `${general.replace(/\/+$/, "")}/v1/logs`);
	if (endpoint === undefined) {
		return undefined;
	}
	try {
		const url = new URL(endpoint);
		if (url.protocol !== "http:" && url.protocol !== "https:") {
			return undefined;
		}
	} catch {
		return undefined;
	}
	const headers: Record<string, string> = {};
	for (const [key, value] of parsePairs(
		nonEmpty(env.OTEL_EXPORTER_OTLP_LOGS_HEADERS) ?? env.OTEL_EXPORTER_OTLP_HEADERS,
	)) {
		headers[key] = value;
	}
	headers["content-type"] = "application/json";
	const resource = parsePairs(env.OTEL_RESOURCE_ATTRIBUTES).filter(([key]) => key !== "service.name");
	resource.push(["service.name", "pi"]);
	return { endpoint, headers, resource };
}

const asRecord = (value: unknown): Record<string, unknown> | undefined =>
	typeof value === "object" && value !== null ? (value as Record<string, unknown>) : undefined;

const name = (value: unknown): string | undefined =>
	typeof value === "string" && value.trim() !== "" ? value.slice(0, MAX_NAME_CHARS) : undefined;

const count = (value: unknown): number | undefined =>
	typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : undefined;

/**
 * The metadata record for one Pi event, or `undefined` for an event that
 * yields none. Reads only the fields it sends.
 */
export function usageRecord(eventName: string, payload: unknown): UsageRecord | undefined {
	const event = asRecord(payload);
	const attributes: Record<string, AttributeValue> = {};
	const put = (key: string, value: AttributeValue | undefined) => {
		if (value !== undefined) {
			attributes[key] = value;
		}
	};
	switch (eventName) {
		case "session_start":
			return { name: "pi.session_start", attributes };
		case "session_shutdown":
			return { name: "pi.session_end", attributes };
		case "agent_start":
			return { name: "pi.agent_start", attributes };
		case "agent_end":
			return { name: "pi.agent_end", attributes };
		case "message_end": {
			const message = asRecord(event?.message);
			if (message?.role !== "assistant") {
				return undefined;
			}
			const usage = asRecord(message.usage);
			put("pi.model", name(message.model));
			put("pi.provider", name(message.provider));
			put("pi.tokens.input", count(usage?.input));
			put("pi.tokens.output", count(usage?.output));
			put("pi.tokens.cache_read", count(usage?.cacheRead));
			put("pi.tokens.cache_write", count(usage?.cacheWrite));
			put("pi.cost.total", count(asRecord(usage?.cost)?.total));
			return { name: "pi.model_usage", attributes };
		}
		case "tool_execution_end": {
			put("pi.tool.name", name(event?.toolName));
			if (typeof event?.isError === "boolean") {
				put("pi.tool.success", !event.isError);
			}
			put("pi.tool.duration_ms", count(event?.durationMs));
			return { name: "pi.tool_result", attributes };
		}
		default:
			return undefined;
	}
}

/** The Pi events {@link usageRecord} turns into records. */
export const TELEMETRY_EVENTS = [
	"session_start",
	"agent_start",
	"message_end",
	"tool_execution_end",
	"agent_end",
	"session_shutdown",
] as const;

const otlpValue = (value: AttributeValue) => {
	if (typeof value === "boolean") {
		return { boolValue: value };
	}
	if (typeof value === "number") {
		return Number.isInteger(value) ? { intValue: String(value) } : { doubleValue: value };
	}
	return { stringValue: value };
};

const otlpAttributes = (entries: Array<[string, AttributeValue]>) =>
	entries.map(([key, value]) => ({ key, value: otlpValue(value) }));

/** An OTLP/JSON `ExportLogsServiceRequest` for `records`, each with its time in milliseconds. */
export function encodeLogs(config: TelemetryConfig, records: Array<UsageRecord & { timeMs: number }>): string {
	return JSON.stringify({
		resourceLogs: [
			{
				resource: { attributes: otlpAttributes(config.resource) },
				scopeLogs: [
					{
						scope: { name: "dot-agent-deck.pi" },
						logRecords: records.map((record) => {
							const time = (BigInt(Math.trunc(record.timeMs)) * 1_000_000n).toString();
							return {
								timeUnixNano: time,
								observedTimeUnixNano: time,
								body: { stringValue: record.name },
								attributes: otlpAttributes([
									["event.name", record.name],
									...Object.entries(record.attributes),
								]),
							};
						}),
					},
				],
			},
		],
	});
}

export type FetchLike = (
	url: string,
	init: { method: string; headers: Record<string, string>; body: string; signal: AbortSignal; redirect: "error" },
) => Promise<unknown>;

/** The exporter for one extension instance. */
export class PiTelemetry {
	private pending: Array<UsageRecord & { timeMs: number }> = [];
	private timer: ReturnType<typeof setInterval> | undefined;
	/** The one flush worker, while it runs: it sends batches until none waits. */
	private worker: Promise<void> | undefined;
	/** Cancels the request that is out, if one is. */
	private active: AbortController | undefined;
	/** Aborts when the shutdown's time is up; set by {@link shutdown}. */
	private deadline: AbortSignal | undefined;
	/** Records dropped because {@link MAX_PENDING} were waiting. */
	dropped = 0;

	private readonly config: TelemetryConfig;
	private readonly fetchImpl: FetchLike;
	private readonly now: () => number;

	constructor(config: TelemetryConfig, fetchImpl: FetchLike, now: () => number = Date.now, intervalMs: number = BATCH_INTERVAL_MS) {
		this.config = config;
		this.fetchImpl = fetchImpl;
		this.now = now;
		if (intervalMs > 0) {
			this.timer = setInterval(() => {
				void this.flush();
			}, intervalMs);
			this.timer.unref?.();
		}
	}

	/** Record what `eventName` says, if anything. Never throws. */
	observe(eventName: string, payload: unknown): void {
		try {
			const record = usageRecord(eventName, payload);
			if (record === undefined) {
				return;
			}
			if (this.pending.length >= MAX_PENDING) {
				this.dropped += 1;
				return;
			}
			this.pending.push({ ...record, timeMs: this.now() });
		} catch {
			// Telemetry never affects Pi.
		}
	}

	/**
	 * Send what is waiting. A flush while one runs joins it, and that one sends
	 * what arrived meanwhile once its request is done. Never rejects.
	 */
	flush(): Promise<void> {
		if (this.worker === undefined) {
			this.worker = this.drain().finally(() => {
				this.worker = undefined;
			});
		}
		return this.worker;
	}

	/**
	 * The session is ending: stop the timer and send the last batch, waiting
	 * at most {@link SHUTDOWN_TIMEOUT_MS} for it and any request already out.
	 */
	async shutdown(): Promise<void> {
		if (this.timer !== undefined) {
			clearInterval(this.timer);
			this.timer = undefined;
		}
		if (this.deadline === undefined) {
			this.deadline = AbortSignal.timeout(SHUTDOWN_TIMEOUT_MS);
			this.deadline.addEventListener("abort", () => this.active?.abort(), { once: true });
		}
		const deadline = this.deadline;
		const timeUp = new Promise<void>((resolve) => {
			if (deadline.aborted) {
				resolve();
			} else {
				deadline.addEventListener("abort", () => resolve(), { once: true });
			}
		});
		await Promise.race([this.flush(), timeUp]);
	}

	private async drain(): Promise<void> {
		while (this.pending.length > 0 && this.deadline?.aborted !== true) {
			await this.send();
		}
	}

	private async send(): Promise<void> {
		const batch = this.pending;
		this.pending = [];
		const active = new AbortController();
		this.active = active;
		try {
			const signals = [active.signal, AbortSignal.timeout(EXPORT_TIMEOUT_MS)];
			if (this.deadline !== undefined) {
				signals.push(this.deadline);
			}
			await this.fetchImpl(this.config.endpoint, {
				method: "POST",
				headers: this.config.headers,
				body: encodeLogs(this.config, batch),
				signal: AbortSignal.any(signals),
				redirect: "error",
			});
		} catch {
			// Dropped: a backend that is down, slow or redirecting never affects Pi.
		} finally {
			this.active = undefined;
		}
	}
}

/** The exporter `env` asks for, or `undefined` when it asks for none. */
export function createPiTelemetry(env: Env, fetchImpl?: FetchLike): PiTelemetry | undefined {
	const config = telemetryConfig(env);
	if (config === undefined) {
		return undefined;
	}
	const send = fetchImpl ?? (globalThis.fetch as unknown as FetchLike | undefined);
	return send === undefined ? undefined : new PiTelemetry(config, send);
}
