/**
 * dot-agent-deck orchestrator extension for Pi (PRD #201, M2.1 + M2.2).
 *
 * This is the thin Pi-API glue. It:
 *   1. registers `delegate` and `work_done` as native, schema-validated tools
 *      whose bodies shell the `dot-agent-deck` CLI, and
 *   2. subscribes to Pi's event bus and reports the pane's status
 *      (running / waiting / finished) and card detail (directory, submitted
 *      prompt, tool calls — issue #622) via `dot-agent-deck agent-event` — so
 *      a Pi pane's card is driven with NO Claude-Code hook installed and NO
 *      `~/.claude/settings.json` mutation.
 *
 * All the testable decisions (argv construction, event→state mapping, error
 * classification) live in the pure, import-free `./orchestrator.ts`. This file
 * only wires them to Pi. The CLI routes over the daemon socket using the pane
 * env vars the daemon already injects (DOT_AGENT_DECK_PANE_ID / _AGENT_ID /
 * _VIA_DAEMON); the extension does not set them.
 *
 * The `@earendil-works/pi-coding-agent` and `typebox` imports are resolved from
 * Pi's own runtime when the extension is loaded (Pi loads extensions via jiti),
 * which is why they are not dependencies of this package.
 */

import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import {
	buildAgentEventArgv,
	buildDelegateArgv,
	buildGetSeedArgv,
	buildWorkDoneArgv,
	createSerialQueue,
	execFailureMessage,
	isAgentState,
	isUnsupportedFlagFailure,
	legacyAgentEventArgv,
	piEventReport,
	resolveDeckBin,
	SEED_DELIVER_AS,
	seedToDeliver,
	spawnFailureMessage,
} from "./orchestrator.ts";

/**
 * The deck CLI this pane shells: the absolute path the spawning deck exported
 * in `DOT_AGENT_DECK_EXE`, or the bare name for an older deck that did not
 * (issue #1385). Read once — the value is fixed for the life of the process.
 */
const deckBin = resolveDeckBin(process.env);

/**
 * Shell `dot-agent-deck <argv>` via Pi's exec helper. Throws a clear Error on a
 * spawn failure (missing binary) or a non-zero exit, so tool callers surface
 * `isError` to the LLM. Returns the exec result on success.
 */
async function runDeck(
	pi: ExtensionAPI,
	argv: string[],
	signal: AbortSignal | undefined,
) {
	let outcome: { code: number; stdout: string; stderr: string; killed: boolean };
	try {
		outcome = await pi.exec(deckBin, argv, { signal });
	} catch (err) {
		throw new Error(spawnFailureMessage(argv, err, deckBin));
	}
	const failure = execFailureMessage(argv, outcome, deckBin);
	if (failure) {
		throw new DeckExecError(failure, outcome);
	}
	return outcome;
}

/** A non-zero exit from the deck CLI, keeping the exec result it came from. */
class DeckExecError extends Error {
	readonly outcome: { code: number; stdout: string; stderr: string };

	constructor(message: string, outcome: { code: number; stdout: string; stderr: string }) {
		super(message);
		this.outcome = outcome;
	}
}

export default function orchestratorExtension(pi: ExtensionAPI): void {
	// --- M2.1: native delegate tool --------------------------------------
	pi.registerTool({
		name: "delegate",
		label: "Delegate",
		description:
			"Delegate a task to a worker role in the current dot-agent-deck orchestration. " +
			"The dot-agent-deck daemon routes the task to that role's agent pane. Orchestrator panes only.",
		promptSnippet: "Delegate a task to a dot-agent-deck worker role",
		promptGuidelines: [
			"Use delegate to hand a scoped task to a worker role instead of doing the work yourself when you are the orchestrator.",
		],
		parameters: Type.Object({
			role: Type.String({ description: 'Worker role name to delegate to (e.g. "coder").' }),
			task: Type.String({
				description: "Full task description with the context, file paths, and constraints the worker needs.",
			}),
		}),
		async execute(_toolCallId, params, signal) {
			const argv = buildDelegateArgv(params.role, params.task);
			await runDeck(pi, argv, signal);
			return {
				content: [{ type: "text", text: `Delegated task to role "${params.role}".` }],
				details: { role: params.role },
			};
		},
	});

	// --- M2.1: native work-done tool -------------------------------------
	pi.registerTool({
		name: "work_done",
		label: "Work Done",
		description:
			"Signal task completion back to the orchestrator via dot-agent-deck, with a summary of what was accomplished.",
		promptSnippet: "Report task completion back to the orchestrator",
		promptGuidelines: [
			"Use work_done when you have finished the delegated task, passing a concise summary of what changed.",
		],
		parameters: Type.Object({
			summary: Type.String({ description: "Summary of what was accomplished, including file paths and outcomes." }),
			done: Type.Optional(
				Type.Boolean({
					description: "Set true ONLY to signal the entire orchestration is complete (orchestrator only).",
				}),
			),
		}),
		async execute(_toolCallId, params, signal) {
			const argv = buildWorkDoneArgv(params.summary, params.done ?? false);
			await runDeck(pi, argv, signal);
			return {
				content: [{ type: "text", text: "Reported work-done to dot-agent-deck." }],
				details: { done: params.done ?? false },
			};
		},
	});

	// --- M2.2 / issue #622: event bus → card reports ---------------------
	// Reporting is best-effort: a failed report (e.g. no pane env vars, daemon
	// down) must never break the agent loop, so failures are swallowed here —
	// unlike the tools above, which surface errors to the LLM.
	//
	// `legacyCli` is set once a CLI has refused the detail flags as unknown
	// (`isUnsupportedFlagFailure`) and accepted the bare lifecycle argv — an
	// older deck (see `legacyAgentEventArgv`). From then on only lifecycle
	// reports are sent, bare, so status keeps working and no report is spent on
	// a flag that CLI cannot read. Any other failure (no daemon, a transient
	// socket error) still gets the bare retry for that one report but leaves
	// the session's detail reporting on.
	let legacyCli = false;
	// Every report, with its retry, runs to completion before the next one
	// starts, so the deck receives them in the order Pi emitted them.
	const inOrder = createSerialQueue();
	const report = (eventName: string, event: unknown, ctx: ExtensionContext): Promise<void> =>
		inOrder(() => reportNow(eventName, event, ctx));
	const reportNow = async (eventName: string, event: unknown, ctx: ExtensionContext): Promise<void> => {
		const decided = piEventReport(eventName, event, ctx.cwd);
		if (!decided) {
			return;
		}
		const fallback = legacyAgentEventArgv(decided);
		if (legacyCli) {
			if (isAgentState(decided.type)) {
				await runDeck(pi, fallback ?? buildAgentEventArgv(decided.type), ctx.signal).catch(() => {});
			}
			return;
		}
		try {
			await runDeck(pi, buildAgentEventArgv(decided.type, decided.detail), ctx.signal);
		} catch (err) {
			// Best-effort. Retry a lifecycle report the way an older CLI reads it.
			if (fallback) {
				try {
					await runDeck(pi, fallback, ctx.signal);
					legacyCli = err instanceof DeckExecError && isUnsupportedFlagFailure(err.outcome);
				} catch {
					// Intentionally ignored — card reporting is best-effort.
				}
			}
		}
	};

	// --- PRD #201: NATIVE prompt delivery on session_start ----------------
	// Pull the seed/prompt the daemon prepared for this pane (`get-seed`) and,
	// if there is one, deliver it via `pi.sendUserMessage` — which "always
	// triggers a turn". This replaces the daemon typing the prompt into the
	// pane's PTY (the last workaround): status, delegation, AND now prompt
	// delivery are all native for a Pi pane. Best-effort like status — a
	// failure here (no binary, no daemon, older daemon that doesn't answer)
	// must never break the agent loop; the daemon's PTY-injection safety net
	// still delivers, so we swallow errors and simply no-send.
	const deliverPendingSeed = async (ctx: ExtensionContext): Promise<void> => {
		const argv = buildGetSeedArgv();
		let outcome: { code: number; stdout: string; stderr: string; killed: boolean };
		try {
			outcome = await pi.exec(deckBin, argv, { signal: ctx.signal });
		} catch {
			// Spawn failure (missing binary / socket) — safety net covers it.
			return;
		}
		// `get-seed` exits 0 even with no seed; a non-zero exit means the
		// request failed, so treat it as "no seed" and let the fallback deliver.
		if (execFailureMessage(argv, outcome, deckBin)) {
			return;
		}
		const seed = seedToDeliver(outcome.stdout);
		if (!seed) {
			return;
		}
		pi.sendUserMessage(seed, { deliverAs: SEED_DELIVER_AS });
	};

	pi.on("session_start", async (event, ctx) => {
		// Report status first (Idle — a Pi pane awaiting its first prompt is
		// idle, matching every other backend's session-start), then deliver the
		// native seed, which triggers a turn (→ agent_start → Thinking) — the
		// real Idle→running transition the live-pane e2e asserts.
		await report("session_start", event, ctx);
		await deliverPendingSeed(ctx);
	});
	// Issue #622: the prompt lands before the agent loop begins, so `Prmt:`
	// appears as the turn starts; each tool call shows as the active tool and
	// counts once it finishes. Awaited like the lifecycle reports, so a tool's
	// end can never reach the deck ahead of its start.
	pi.on("before_agent_start", async (event, ctx) => {
		await report("before_agent_start", event, ctx);
	});
	pi.on("agent_start", async (event, ctx) => {
		await report("agent_start", event, ctx);
	});
	pi.on("tool_execution_start", async (event, ctx) => {
		await report("tool_execution_start", event, ctx);
	});
	pi.on("tool_execution_end", async (event, ctx) => {
		await report("tool_execution_end", event, ctx);
	});
	pi.on("agent_settled", async (event, ctx) => {
		await report("agent_settled", event, ctx);
	});
	pi.on("session_shutdown", async (event, ctx) => {
		await report("session_shutdown", event, ctx);
	});
}
