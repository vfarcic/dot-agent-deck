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
 * loop, each request gives up after {@link EXPORT_TIMEOUT_MS}, the last batch
 * is sent when the session shuts down, and a failure is dropped silently.
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

/** How long the last export may take as the session shuts down. */
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

/** Where and how to export, or `undefined` when this extension should not. */
export function telemetryConfig(env: Env): TelemetryConfig | undefined {
	if (env[TELEMETRY_ENV] !== "1") {
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

export type FetchLike = (url: string, init: { method: string; headers: Record<string, string>; body: string; signal: AbortSignal }) => Promise<unknown>;

/** The exporter for one extension instance. */
export class PiTelemetry {
	private pending: Array<UsageRecord & { timeMs: number }> = [];
	private timer: ReturnType<typeof setInterval> | undefined;
	private inFlight: Promise<void> = Promise.resolve();
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

	/** Send what is waiting, after any export already in flight. Never rejects. */
	flush(timeoutMs: number = EXPORT_TIMEOUT_MS): Promise<void> {
		this.inFlight = this.inFlight.then(() => this.send(timeoutMs));
		return this.inFlight;
	}

	/** The session is ending: stop the timer and send the last batch. */
	async shutdown(): Promise<void> {
		if (this.timer !== undefined) {
			clearInterval(this.timer);
			this.timer = undefined;
		}
		await this.flush(SHUTDOWN_TIMEOUT_MS);
	}

	private async send(timeoutMs: number): Promise<void> {
		if (this.pending.length === 0) {
			return;
		}
		const batch = this.pending;
		this.pending = [];
		try {
			await this.fetchImpl(this.config.endpoint, {
				method: "POST",
				headers: this.config.headers,
				body: encodeLogs(this.config, batch),
				signal: AbortSignal.timeout(timeoutMs),
			});
		} catch {
			// Dropped: a backend that is down never affects Pi.
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
