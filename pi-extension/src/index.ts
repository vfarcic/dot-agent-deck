/**
 * dot-agent-deck orchestrator extension for Pi (PRD #201, M2.1 + M2.2).
 *
 * This is the thin Pi-API glue. It:
 *   1. registers `delegate` and `work_done` as native, schema-validated tools
 *      whose bodies shell the `dot-agent-deck` CLI, and
 *   2. subscribes to Pi's lifecycle event bus and reports the pane's status
 *      (running / waiting / finished) via `dot-agent-deck agent-event` — so a
 *      Pi pane is status-tracked with NO Claude-Code hook installed and NO
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
	buildWorkDoneArgv,
	DECK_BIN,
	execFailureMessage,
	piEventToAgentState,
	spawnFailureMessage,
} from "./orchestrator.ts";

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
		outcome = await pi.exec(DECK_BIN, argv, { signal });
	} catch (err) {
		throw new Error(spawnFailureMessage(argv, err));
	}
	const failure = execFailureMessage(argv, outcome);
	if (failure) {
		throw new Error(failure);
	}
	return outcome;
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

	// --- M2.2: event bus → status mapping --------------------------------
	// Status is best-effort: a failed report (e.g. no pane env vars, daemon
	// down) must never break the agent loop, so failures are swallowed here —
	// unlike the tools above, which surface errors to the LLM.
	const reportStatus = async (eventName: string, ctx: ExtensionContext): Promise<void> => {
		const state = piEventToAgentState(eventName);
		if (!state) {
			return;
		}
		try {
			await runDeck(pi, buildAgentEventArgv(state), ctx.signal);
		} catch {
			// Intentionally ignored — status reporting is best-effort.
		}
	};

	pi.on("session_start", async (_event, ctx) => {
		await reportStatus("session_start", ctx);
	});
	pi.on("agent_start", async (_event, ctx) => {
		await reportStatus("agent_start", ctx);
	});
	pi.on("agent_settled", async (_event, ctx) => {
		await reportStatus("agent_settled", ctx);
	});
	pi.on("session_shutdown", async (_event, ctx) => {
		await reportStatus("session_shutdown", ctx);
	});
}
