import type { DaemonOrchestration, DeckDirectoryEntry, DeckSnapshot } from "../types";

/** A browser-only fleet large enough to expose every voice paging boundary. */
export const VOICE_PAGES_HOME = "/home/dev";
export const VOICE_PAGES_DIRECTORY_NAMES = Array.from({ length: 30 }, (_, index) =>
  index === 27 ? "docs" : `folder-${String(index + 1).padStart(2, "0")}`,
);

export function voicePagesFleet(base: DeckSnapshot): DeckSnapshot[] {
  const connected = Array.from({ length: 6 }, (_, index): DeckSnapshot => {
    const deckId = index === 0 ? base.connection.deckId! : `dev@voice-page-${index + 1}`;
    const agents = index === 0 ? base.agents : base.agents.slice(0, 2);
    return {
      ...base,
      runId: `run_voice_pages_${index}`,
      connection: { status: "connected", deckId, socketPath: deckId, message: "Daemon responding", deckKind: index === 0 ? "local" : "remote" },
      agents: agents.map((agent) => ({ ...agent, daemonId: deckId })),
      totalNodes: agents.length,
    };
  });
  const unusableId = "dev@voice-page-offline";
  return [...connected, {
    ...base,
    runId: "run_voice_pages_offline",
    connection: { status: "disconnected", deckId: unusableId, socketPath: unusableId, message: "No daemon is listening on the configured socket.", deckKind: "remote" },
    agents: [],
    totalNodes: 0,
  }];
}

/** The home listing is long; child folders are empty so browsing remains deterministic. */
export function voicePagesDirectory(path: string): { path: string; parent?: string; entries: DeckDirectoryEntry[] } | undefined {
  if (path === VOICE_PAGES_HOME) return {
    path,
    parent: "/home",
    entries: VOICE_PAGES_DIRECTORY_NAMES.map((name) => ({ path: `${VOICE_PAGES_HOME}/${name}`, displayName: name, isProject: name === "docs" })),
  };
  if (VOICE_PAGES_DIRECTORY_NAMES.some((name) => path === `${VOICE_PAGES_HOME}/${name}`)) return { path, parent: VOICE_PAGES_HOME, entries: [] };
  return undefined;
}

/** The project gives the mode row enough real selectable chips to overflow a short dialog. */
export function voicePagesOrchestrations(path: string): DaemonOrchestration[] | undefined {
  if (path !== `${VOICE_PAGES_HOME}/docs`) return undefined;
  return Array.from({ length: 12 }, (_, index) => ({
    name: `voice-loop-${index + 1}`,
    displayName: `voice-loop-${index + 1}`,
    default: index === 0,
    roles: [{ name: "planner", displayName: "planner", start: true }],
  }));
}
