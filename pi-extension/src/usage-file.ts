/**
 * PRD #1858: the Pi extension's half of the deck's usage file. The deck
 * names a file in `DOT_AGENT_DECK_USAGE_FILE` for a run it exports
 * telemetry for, made new and readable only by the user; the extension
 * appends one line per assistant reply (`message_end`) and one when Pi starts
 * answering a prompt (`agent_start`), in the format the deck reads
 * (`src/telemetry/accounting/usage_file.rs`, version 1). Named fields only,
 * so no prompt, reply, tool output or error message can reach the file.
 */

import { closeSync, constants, fstatSync, openSync, writeSync } from "node:fs";
import { isAbsoluteOverride } from "./orchestrator.ts";

/**
 * The variable naming the file the deck reads this run's usage from. The
 * deck sets it only when it exports telemetry, and sets it empty otherwise.
 */
export const USAGE_FILE_ENV = "DOT_AGENT_DECK_USAGE_FILE";

/** The deck's usage-file path in `env`, when it is an absolute path. */
export function usageFilePath(env: Record<string, string | undefined>, platform: string): string | undefined {
	const path = env[USAGE_FILE_ENV];
	return path !== undefined && isAbsoluteOverride(path, platform) ? path : undefined;
}

function asRecord(value: unknown): Record<string, unknown> | null {
	return typeof value === "object" && value !== null && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: null;
}

/** A token count: a whole, non-negative number, else `null`, never a 0. */
const usageCount = (value: unknown): number | null =>
	typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;

/** A name the deck's usage file keeps: an identifier, never free text. */
const usageName = (value: unknown): string | undefined =>
	typeof value === "string" && /^[A-Za-z0-9._:/@+-]{1,128}$/.test(value) ? value : undefined;

/**
 * The usage-file line for one Pi `message_end`, or `undefined` for a message
 * that is not an assistant reply. It carries the reply's model, token
 * counts, Pi's own cost estimate and how it stopped.
 *
 * Pi gives every reply all four counts. One that is missing or not a count
 * is written as `null`, which the deck reads as a reply in a format it does
 * not know, never as 0. A reply with no usage at all, or one that failed or
 * was aborted with every count still 0 — the zeros Pi starts a reply with,
 * not a count the provider returned — is written with `"reported": false`
 * and no counts.
 */
export function usageFileLine(event: unknown): string | undefined {
	const message = asRecord(asRecord(event)?.message);
	if (message?.role !== "assistant") {
		return undefined;
	}
	const line: Record<string, unknown> = { v: 1, type: "reply", agent: "pi" };
	const model = usageName(message.model);
	if (model !== undefined) {
		line.model = model;
	}
	const usage = asRecord(message.usage);
	const counts =
		usage === null
			? undefined
			: {
					input: usageCount(usage.input),
					output: usageCount(usage.output),
					cache_read: usageCount(usage.cacheRead),
					cache_write: usageCount(usage.cacheWrite),
				};
	const failed = message.stopReason === "error" || message.stopReason === "aborted";
	const zeros = counts !== undefined && Object.values(counts).every((count) => count === 0);
	if (counts === undefined || (failed && zeros)) {
		line.reported = false;
	} else {
		Object.assign(line, counts);
		const cost = asRecord(usage?.cost)?.total;
		if (typeof cost === "number" && Number.isFinite(cost) && cost >= 0) {
			line.cost_usd = cost;
		}
	}
	const stop = usageName(message.stopReason);
	if (stop !== undefined) {
		line.stop = stop;
	}
	return JSON.stringify(line);
}

/**
 * The usage-file line for Pi's `agent_start`: a turn began. It adds no usage;
 * it makes a Pi that dies before its next reply ends read as mid-turn rather
 * than as its last completed reply.
 */
export function usageTurnStartLine(): string {
	return JSON.stringify({ v: 1, type: "turn_start", agent: "pi" });
}

/** What {@link appendUsageLine} did. */
export type AppendResult = "appended" | "unsafe" | "failed";

/**
 * Append `line` to the deck's usage file at `path`, only when it is safe: the
 * file must already be there (the deck makes it), and is opened without
 * following a symlink and without waiting on a FIFO; it must be a regular
 * file of this user's that no other user can read or write. Anything else is
 * skipped, and the run's usage is then not recorded. `O_NONBLOCK` keeps a
 * FIFO from holding up the open; the calls are synchronous, so a slow file
 * system still holds Pi up for as long as the open, stat and write take.
 */
export function appendUsageLine(path: string, line: string): AppendResult {
	const flags =
		constants.O_WRONLY | constants.O_APPEND | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0);
	let fd: number;
	try {
		fd = openSync(path, flags);
	} catch {
		return "unsafe";
	}
	try {
		const stat = fstatSync(fd);
		const own = typeof process.getuid !== "function" || stat.uid === process.getuid();
		if (!stat.isFile() || !own || (stat.mode & 0o077) !== 0) {
			return "unsafe";
		}
		writeSync(fd, `${line}\n`);
		return "appended";
	} catch {
		return "failed";
	} finally {
		closeSync(fd);
	}
}
