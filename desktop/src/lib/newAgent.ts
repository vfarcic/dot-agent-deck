import type { AuthoringKind, ConnectionView, DaemonOrchestration, DeckDirectoryEntry, DeckFleet, NewAgentOption, NewAgentOptions, NewAgentOrchestrations } from "../types";
import { DISPLAY_LIMITS, deckName, displayIdentity, displayText } from "./displayText";

/**
 * PRD #1223 M4/M5 — the rules of the New agent flow, kept out of the component
 * so each one is testable as a function.
 */

/**
 * How long the flow waits, after a deck accepted a start, for that deck's fleet
 * entry to list the new agent before it gives up on opening the pane (PRD #1223
 * M5).
 *
 * The pane cannot open earlier: `paneAgentRetired` in `App.tsx` closes a pane
 * whose connected deck does not list its agent. The start action refreshes the
 * target deck directly and nudges its watcher, so the ordinary case is one
 * round trip. The bound is for the slow case, and it is set against the
 * crate's five-second `RECONCILE_INTERVAL` — the longest a watched deck takes to
 * re-read its agent list when nothing else prompts it — with room on top for a
 * remote deck's tunnel.
 */
export const NEW_AGENT_APPEAR_TIMEOUT_MS = 12_000;

/**
 * What a deck in each state that cannot take a spawn says when its connection
 * carries no message of its own. `pending`, `unconfigured`, `disconnected` and
 * `incompatible` are the sentences the overview's degraded group notes fall
 * back to, and those notes read them from here, so the deck step and the
 * overview cannot drift; `loading` is the deck step's short form of the
 * overview's loading note, which carries more than one sentence.
 */
export const DECK_STATE_FALLBACK = {
  pending: "This deck has not reported yet.",
  loading: "Reading the deck's agent list.",
  unconfigured: "This deck has no address yet.",
  disconnected: "No deck is listening on the configured socket.",
  incompatible: "A deck answered but this build cannot speak to it.",
} as const;

/**
 * Why a deck cannot take a spawn, as display text — or `undefined` when it can.
 *
 * A deck can take one when it is connected, which already excludes a pending
 * deck (`loading`), an unconfigured one and an incompatible one (`error`). A
 * deck whose build stamps differ is `error` until the user accepts it through
 * the overview's Connect anyway, and `connected` afterwards — so "compatible
 * or explicitly accepted" is the connected status, with no second flag to read.
 */
export function deckUnavailableReason(connection: ConnectionView): string | undefined {
  const own = connection.message ? displayText(connection.message, DISPLAY_LIMITS.message) : undefined;
  if (connection.pending) return own ?? DECK_STATE_FALLBACK.pending;
  if (connection.unconfigured) return own ?? DECK_STATE_FALLBACK.unconfigured;
  switch (connection.status) {
    case "connected":
      return undefined;
    case "loading":
      return own ?? DECK_STATE_FALLBACK.loading;
    case "disconnected":
      return own ?? DECK_STATE_FALLBACK.disconnected;
    case "error":
      return own ?? DECK_STATE_FALLBACK.incompatible;
  }
}

/** One row of the deck step. */
export interface DeckChoice {
  /** The wire `connection.deckId` — the value every later request carries. */
  deckId: string;
  /** What the deck is called, as display text. */
  name: string;
  deckKind: "local" | "remote";
  /** Why it cannot take a spawn; absent when it can. */
  reason?: string;
}

/**
 * Every deck in the fleet, in fleet order, with the reason each ineligible one
 * gives. An entry with no `deckId` is a placeholder for a fleet that has not
 * arrived, not a deck, and is left out.
 */
export function deckChoices(fleet: DeckFleet): DeckChoice[] {
  return fleet.flatMap((deck) => {
    const deckId = deck.connection.deckId;
    if (deckId === undefined) return [];
    const reason = deckUnavailableReason(deck.connection);
    return [{ deckId, name: deckName(deck.connection), deckKind: deck.connection.deckKind ?? "local", ...(reason === undefined ? {} : { reason }) }];
  });
}

/**
 * The deck the step opens on: the one the flow was opened FROM (a deck
 * header's affordance) when it can take a spawn, otherwise the only eligible
 * deck when there is exactly one, otherwise none — the user chooses.
 */
export function preselectedDeck(choices: readonly DeckChoice[], requested?: string): string | undefined {
  if (requested !== undefined && choices.some((choice) => choice.deckId === requested && choice.reason === undefined)) return requested;
  const eligible = choices.filter((choice) => choice.reason === undefined);
  return eligible.length === 1 ? eligible[0].deckId : undefined;
}

/**
 * The Name field's prefill: the last component of a path the DECK returned —
 * a label, never a path, and never sent back as one. Both separators are
 * accepted because the deck's platform need not be this one. The root, which
 * has no last component, gives an empty name, as the TUI's `file_name()` does.
 */
export function directoryLabel(path: string): string {
  return path.split(/[\\/]+/).filter(Boolean).at(-1) ?? "";
}

/**
 * The crate's refusal of a typed path that is not askable as a directory —
 * `validate_pasted_project_path`'s sentence. The listing applies it to a typed
 * path, and since PRD #1223 audit D2 so does a start naming a directory; the
 * dialog and the fixture bridge repeat it so a client-side refusal reads the
 * same as the crate's.
 */
export const TYPED_PATH_SHAPE_REFUSAL = "enter an absolute directory path, without control characters, that the deck can see";

/**
 * The dialog's cheap pre-check on a typed path (PRD #1223 audit D2): absolute,
 * and free of ASCII controls. The crate makes the real check, per platform —
 * a leading `/` everywhere, plus `C:\`, `C:/` and UNC forms on Windows — so
 * this accepts the union of those shapes and never refuses one the crate would
 * take on any platform. What it exists to stop is `repo`, `./repo` or `~/repo`
 * on a deck without the listing verb, where the typed path is what the start
 * sends.
 */
export function isAbsoluteTypedPath(path: string): boolean {
  return /^(?:\/|\\[\\/]|[A-Za-z]:[\\/])/.test(path) && !/[\u0000-\u001f\u007f]/.test(path);
}

/**
 * The Command field's prefill, in the TUI's order (`resolve_seed_command`):
 * the deck host's configured `default_command`, then the command this app last
 * started a plain agent with on that deck, then blank — which starts the
 * deck's default shell.
 */
export function seedCommand(defaultCommand?: string, lastCommand?: string): string {
  if (defaultCommand) return defaultCommand;
  if (lastCommand?.trim()) return lastCommand;
  return "";
}

/**
 * The authoring Mode chips (PRD #1223 M7), in the TUI cycler's order and with
 * its labels — `schedule`, `schedule: issues`, `dispatcher`.
 */
export const AUTHORING_MODES: readonly { kind: AuthoringKind; label: string }[] = [
  { kind: "schedule", label: "schedule" },
  { kind: "schedule-issues", label: "schedule: issues" },
  { kind: "dispatcher", label: "dispatcher" },
];

/** Why no authoring chip is offered, for a deck that cannot say which ones it can start. */
export const AUTHORING_WITHHELD = {
  unsupported: "This deck does not report which authoring agents it can start, so schedule and dispatcher are not offered.",
  none: "This deck cannot compose authoring seeds, so schedule and dispatcher are not offered.",
} as const;

/**
 * The authoring chips a deck's options offer, or why none are.
 *
 * A chip is offered only when the deck lists its kind in `authoringKinds` —
 * which the deck does exactly when it can compose that seed — and
 * `schedule: issues` only when the deck's experimental flag is on, as the TUI
 * shows that option only with the flag. Nothing is offered while the options
 * are still loading, and a deck older than the options query, or one that
 * lists no kind this app knows, is given the reason instead.
 */
export function authoringModes(options: NewAgentOptions | undefined): { offered: { kind: AuthoringKind; label: string }[]; withheld?: string } {
  if (options === undefined) return { offered: [] };
  if (options.kind === "unsupported") return { offered: [], withheld: AUTHORING_WITHHELD.unsupported };
  const composable = AUTHORING_MODES.filter((mode) => options.authoringKinds.includes(mode.kind));
  if (composable.length === 0) return { offered: [], withheld: AUTHORING_WITHHELD.none };
  return { offered: composable.filter((mode) => mode.kind !== "schedule-issues" || options.experimental) };
}

/**
 * The command an authoring agent starts with — the TUI's
 * `resolve_authoring_command`, applied where the TUI applies it.
 *
 * A typed command is used as it is. A blank one would start the deck's default
 * shell, which cannot act on a seed, so it resolves to the deck host's
 * configured `default_command` (trimmed), and failing that to the default
 * command of the deck's own `claude` registry entry — `claude` when the deck
 * reports none.
 */
export function resolveAuthoringCommand(command: string, defaultCommand: string | undefined, agents: readonly NewAgentOption[]): string {
  if (command.trim()) return command;
  const configured = defaultCommand?.trim();
  if (configured) return configured;
  return agents.find((agent) => agent.id === "claude")?.defaultCommand?.trim() || "claude";
}

/**
 * The orchestration Mode chips (PRD #1223 M6) — the TUI's `[Orch: <name>]`, one
 * per orchestration the chosen directory's project defines on that deck, or why
 * none is offered. Nothing is offered while the answer is loading or for an
 * ordinary directory, and a deck that cannot launch one from this flow gives
 * its reason instead.
 *
 * # Namesakes are shown, never offered (PRD #1223 audit F2)
 *
 * A project may define two orchestrations with one name — config validation
 * only warns — and `ResolveProject` lists both. But the launch identifies an
 * orchestration by its name alone, and the deck's `PrepareWorkflow` takes the
 * FIRST definition with that name: choosing the second would run the first's
 * roles and commands. So every orchestration whose name another one in the
 * project shares is returned under `ambiguous`, in the project's order, for the
 * dialog to show disabled with {@link ambiguousOrchestrationReason}; only a
 * uniquely named one is in `offered`, and so only one can ever be submitted.
 * Names compare exactly, as the deck's lookup does.
 */
export function orchestrationModes(answer: NewAgentOrchestrations | undefined): { offered: DaemonOrchestration[]; ambiguous?: DaemonOrchestration[]; withheld?: string } {
  if (answer === undefined || answer.kind === "not_project") return { offered: [] };
  if (answer.kind === "unsupported") return { offered: [], withheld: answer.reason };
  const uses = new Map<string, number>();
  for (const orchestration of answer.orchestrations) uses.set(orchestration.name, (uses.get(orchestration.name) ?? 0) + 1);
  const offered = answer.orchestrations.filter((orchestration) => uses.get(orchestration.name) === 1);
  const ambiguous = answer.orchestrations.filter((orchestration) => uses.get(orchestration.name) !== 1);
  return ambiguous.length === 0 ? { offered } : { offered, ambiguous };
}

/** Why a namesake orchestration's chip is disabled (PRD #1223 audit F2). */
export function ambiguousOrchestrationReason(displayName: string): string {
  return `This project defines more than one orchestration named ${displayText(displayName, DISPLAY_LIMITS.name)}; rename one to launch it here.`;
}

/** The Mode chip id of an orchestration — distinct from every authoring kind and from `none`. */
export function orchestrationModeId(name: string): `orch:${string}` {
  return `orch:${name}`;
}

/**
 * The titles of the orchestrations live on one deck — the TUI's
 * `live_orchestration_cwds_and_titles`, read from that deck's own fleet entry:
 * each orchestration role's title when it has one, else the orchestration's
 * name, which is what its tab shows. Duplicates are dropped, so a run's many
 * roles count once.
 */
export function liveOrchestrationTitles(fleet: DeckFleet, deckId: string): string[] {
  const titles = new Set<string>();
  for (const deck of fleet) {
    if (deck.connection.deckId !== deckId) continue;
    for (const agent of deck.agents) {
      if (agent.tab.kind !== "orchestration") continue;
      titles.add(agent.tab.displayTitle ? agent.tab.displayTitle : agent.tab.name);
    }
  }
  return [...titles];
}

/** The directories live orchestrations run in on one deck — the TUI's same-directory warning reads these. */
export function liveOrchestrationDirectories(fleet: DeckFleet, deckId: string): string[] {
  const directories = new Set<string>();
  for (const deck of fleet) {
    if (deck.connection.deckId !== deckId) continue;
    for (const agent of deck.agents) {
      if (agent.tab.kind === "orchestration" && agent.tab.cwd) directories.add(agent.tab.cwd);
    }
  }
  return [...directories];
}

/**
 * The Name prefill while an orchestration is selected — the TUI's
 * `suggest_orchestration_name`: `<basename>-orchestrator-N` for the lowest `N`
 * no live orchestration's title on that deck already holds. Counted over the
 * deck's live orchestrations, not per directory, because uniqueness is the
 * point of the name.
 */
export function suggestOrchestrationName(basename: string, liveTitles: readonly string[]): string {
  for (let n = 1; ; n += 1) {
    const candidate = `${basename}-orchestrator-${n}`;
    if (!liveTitles.includes(candidate)) return candidate;
  }
}

/**
 * The title a launch will actually take — the TUI's `resolved_title`: the Name
 * when it is not empty, otherwise the orchestration's own name, which is what
 * the tab falls back to. The collision check compares THIS, never the raw
 * field, since an empty Name is not "no title".
 */
export function orchestrationRunTitle(name: string, orchestration: string): string {
  return name === "" ? orchestration : name;
}

/** The TUI's `NAME_COLLISION_WARNING`, for the deck the run would start on. */
export const ORCHESTRATION_TITLE_TAKEN = "This name is already in use by a live orchestration on this deck.";

/** The TUI's `SAME_CWD_ORCHESTRATION_WARNING` — a warning, not a refusal. */
export const SAME_DIRECTORY_ORCHESTRATION = "This directory already runs an orchestration on this deck. Both share its .dot-agent-deck role files and one working tree.";

/**
 * How many unconfirmed roles a cleanup warning names before it counts the rest
 * (PRD #1223 audit V7). A rollback of a 64-role orchestration can leave more
 * names than any one alert should carry; what the reader needs on screen is the
 * count, the instruction, and enough names to start with.
 */
export const CLEANUP_WARNING_MAX_NAMES = 8;

/**
 * What a role whose name renders as nothing at all is listed as (PRD #1223
 * audit W5).
 *
 * A list item is the one place a blank identity is worse than useless: an empty
 * `<li>` under "2 roles may still be running" leaves the reader counting bullets
 * to work out that one of them was named, which is exactly when they need to go
 * and find it on the deck.
 */
export const UNNAMED_CLEANUP_ROLE = "unnamed role";

/** The parts of the alert a failed launch shows — see {@link cleanupWarning}. */
export type CleanupWarning = {
  /** The count and what to do about it. Never clamped away. */
  summary: string;
  /**
   * The first {@link CLEANUP_WARNING_MAX_NAMES} roles, each clamped as a name
   * and each guaranteed to render as something — see
   * {@link UNNAMED_CLEANUP_ROLE}.
   */
  names: string[];
  /** How many roles are not in `names`; `0` when they all are. */
  overflow: number;
};

/**
 * PRD #1223 audit F6 — the alert a failed launch shows, on its own and before
 * the error sentence, when its rollback could not confirm every role stopped.
 *
 * Structured rather than one sentence (audit V7): the names were joined and the
 * whole thing clamped to `DISPLAY_LIMITS.message`, so a rollback of several
 * long-named roles lost the later identities with nothing saying so. Each name
 * is now clamped on its own and rendered as a list item, and the ones past the
 * cap are COUNTED rather than dropped silently.
 *
 * Each name is an IDENTITY (audit W5), so it goes through `displayIdentity`
 * and not `displayText`. `displayText` retains default-ignorable characters —
 * deliberately, since stripping them would corrupt emoji sequences and Persian,
 * Arabic and Indic orthography — so a role named only of them rendered as an
 * empty list item, which is a blank line where the reader most needs a name.
 */
export function cleanupWarning(unconfirmedStops: readonly string[]): CleanupWarning {
  const count = unconfirmedStops.length;
  const subject = count === 1 ? "1 role" : `${count} roles`;
  const it = count === 1 ? "it" : "them";
  return {
    summary: displayText(`${subject} may still be running on this deck: the rollback could not confirm ${it} stopped. Check the deck and stop ${it} there.`, DISPLAY_LIMITS.message),
    names: unconfirmedStops.slice(0, CLEANUP_WARNING_MAX_NAMES).map((role) => displayIdentity(role, DISPLAY_LIMITS.name, UNNAMED_CLEANUP_ROLE)),
    overflow: Math.max(0, count - CLEANUP_WARNING_MAX_NAMES),
  };
}

/**
 * The crate's `DeckScope::resolve` refusal for a deck this app no longer
 * observes (`src-tauri/src/dto.rs`), which the fixture bridge repeats verbatim.
 * It is the WHOLE message on every path that raises it — the resolve runs
 * before anything else and nothing wraps its error — so it is matched at the
 * start of the message and nowhere else.
 */
const DECK_GONE_PREFIX = "that deck is not one this app is observing:";

/**
 * Whether a refusal means the chosen deck has left the fleet. The flow returns
 * to the deck step on it rather than retargeting another deck.
 *
 * A LEADING match, not a substring one (PRD #1223 audit V3). Failure sentences
 * interpolate project-controlled names — an orchestration's roles above all —
 * so a project whose role is named after this refusal could otherwise make any
 * failure of its own launch read as deck loss, which drops the structured
 * cleanup warning and sends the flow back to the deck step.
 */
export function isDeckGoneError(message: string): boolean {
  return message.startsWith(DECK_GONE_PREFIX);
}

/** Whether `deckId`'s fleet entry lists `agentId` — the composite identity, never the bare id. */
export function fleetLists(fleet: DeckFleet, deckId: string, agentId: string): boolean {
  return fleet.some((deck) => deck.connection.deckId === deckId && deck.agents.some((agent) => agent.id === agentId));
}

/** The directory step's filter: a case-insensitive substring of the entry's name, as the TUI picker's `refilter`. */
export function filterDirectoryEntries(entries: readonly DeckDirectoryEntry[], filter: string): DeckDirectoryEntry[] {
  const query = filter.toLowerCase();
  return query ? entries.filter((entry) => entry.displayName.toLowerCase().includes(query)) : [...entries];
}

/** The subset of a `KeyboardEvent` {@link isNewAgentShortcut} reads, so a test can pass a plain object. */
export interface ShortcutKeyEvent {
  key: string;
  ctrlKey?: boolean;
  metaKey?: boolean;
  altKey?: boolean;
  shiftKey?: boolean;
}

/**
 * Ctrl+N, or Cmd+N — the TUI's `Ctrl+n`. Either modifier on every platform, for
 * `zoomIntentFromKey`'s reason: no other binding here uses `Ctrl N` on macOS or
 * `Cmd N` elsewhere, so reading the platform to pick one buys nothing. Alt and
 * Shift are excluded, so `Ctrl Shift N` and `Alt Cmd N` stay the platform's.
 */
export function isNewAgentShortcut(event: ShortcutKeyEvent): boolean {
  return (event.ctrlKey === true || event.metaKey === true) && event.altKey !== true && event.shiftKey !== true && event.key.toLowerCase() === "n";
}
