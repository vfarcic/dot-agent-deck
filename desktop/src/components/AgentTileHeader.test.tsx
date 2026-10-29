import { render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, createFixtureStartedAgent } from "../data/fixture";
import { mapDesktopSnapshot, type DesktopSnapshotDto } from "../lib/bridge";
import type { AgentSession, RuntimeMode } from "../types";
import { AgentTile } from "./AgentTile";

vi.mock("./TerminalViewport", () => ({ TerminalViewport: () => <pre>terminal</pre> }));

// Issue #2001: the tile's header names the agent, says its type once, and
// prints nothing for a value the deck does not have. The live records come
// through `mapDesktopSnapshot`, the projection a connected deck renders.

type DtoAgent = DesktopSnapshotDto["agents"][number];

function liveAgent(overrides: Partial<DtoAgent>): AgentSession {
  const agent = {
    id: "7", paneId: "pane-7", displayName: "claude", agentType: "claude_code" as const, cliName: "claude",
    incarnation: "header-process-a", rows: 24, cols: 80, status: "working" as const, toolCount: 3,
    cwd: "/work/project", lastUserPrompt: "inspect the fixture", tab: { kind: "dashboard" as const },
    ...overrides,
  } as DtoAgent;
  return mapDesktopSnapshot({
    connection: { status: "connected", deckId: "deck-0000000000002001", socketPath: "/tmp/test.sock",
      deckKind: "local", clientProtocolVersion: 11, clientBuildVersion: "test" },
    agents: [agent], protocolVersion: 11, source: "daemon",
  }).agents[0];
}

function tile(agent: AgentSession, mode: RuntimeMode = "live") {
  render(<AgentTile agent={agent} presentation="tile" showDetails={false} mode={mode}
    selected={false} tab="terminal" evidence={[]} onSelect={vi.fn()}
    onTabChange={vi.fn()} onTerminalInput={vi.fn(async () => {})}
    onTerminalResize={vi.fn(async () => {})} onEvidenceSelect={vi.fn()}
    onRename={vi.fn(async () => {})} />);
  return screen.getByRole("article");
}

/**
 * How many times `word` appears, ignoring case. Not by word boundary:
 * `textContent` runs adjacent elements together (`runningclaude`).
 */
function occurrences(text: string | null, word: string): number {
  return (text ?? "").toLowerCase().split(word.toLowerCase()).length - 1;
}

describe("the agent tile's header (issue #2001)", () => {
  /// Scenario: A live Claude agent the user renamed "reviewer" is on the deck; its tile's title reads "reviewer", and the agent type appears once beside it.
  it("titles a renamed agent with its name and names its type once", () => {
    const card = tile(liveAgent({ displayName: "reviewer" }));
    expect(within(card).getByRole("heading", { level: 2 })).toHaveTextContent(/^reviewer$/);
    const header = card.querySelector("header");
    expect(occurrences(header?.textContent ?? null, "claude")).toBe(1);
  });

  /// Scenario: A live Claude agent started without a name, which the daemon names after its command, is on the deck; its tile's title is that name and the type is not printed a second time under it.
  it("does not repeat the type under an agent named after its command", () => {
    const card = tile(liveAgent({ displayName: "claude" }));
    expect(within(card).getByRole("heading", { level: 2 })).toHaveTextContent(/^claude$/);
    expect(occurrences(card.querySelector("header")?.textContent ?? null, "claude")).toBe(1);
  });

  /// Scenario: A live agent the user named "reviewer" is on the deck from a daemon that reported no binary for it; the line under its title still names its type, Claude code.
  it("keeps the type when the daemon reported no binary", () => {
    const card = tile(liveAgent({ displayName: "reviewer", cliName: undefined }));
    expect(within(card).getByRole("heading", { level: 2 })).toHaveTextContent(/^reviewer$/);
    expect(occurrences(card.querySelector("header")?.textContent ?? null, "claude code")).toBe(1);
  });

  /// Scenario: A live agent is on the deck, where the daemon reports no model and the agent holds no lease; nothing on its tile reads "Unavailable".
  it("prints no Unavailable for the model or the lease on a live tile", () => {
    const card = tile(liveAgent({}));
    expect(card).not.toHaveTextContent(/unavailable/i);
  });

  /// Scenario: A live agent runs in a directory that is literally named "Unavailable"; its tile still prints that directory, because the tile leaves out values the deck lacks rather than words that look like "Unavailable".
  it("still prints a working directory that happens to be named Unavailable", () => {
    const card = tile(liveAgent({ cwd: "Unavailable" }));
    expect(card.querySelector("footer")).toHaveTextContent("Unavailable");
  });

  /// Scenario: An orchestration's coder role is on the deck under its default name; the title reads "coder" and the role is not printed again under it.
  it("titles an orchestration role with its name and does not repeat the role", () => {
    const card = tile(liveAgent({
      displayName: "coder",
      tab: { kind: "orchestration", name: "team", roleIndex: 1, roleName: "coder", isStartRole: false },
    }));
    expect(within(card).getByRole("heading", { level: 2 })).toHaveTextContent(/^coder$/);
    expect(occurrences(card.querySelector("header")?.textContent ?? null, "coder")).toBe(1);
    expect(occurrences(card.querySelector("header")?.textContent ?? null, "claude")).toBe(1);
  });

  /// Scenario: An orchestration's coder role the user renamed "lead" is on the deck; the title reads "lead" and the line under it still says it is the Coder role, running claude.
  it("keeps a renamed role's role name under its new name", () => {
    const card = tile(liveAgent({
      displayName: "lead",
      tab: { kind: "orchestration", name: "team", roleIndex: 1, roleName: "coder", isStartRole: false },
    }));
    expect(within(card).getByRole("heading", { level: 2 })).toHaveTextContent(/^lead$/);
    const header = card.querySelector("header")?.textContent ?? null;
    expect(occurrences(header, "coder")).toBe(1);
    expect(occurrences(header, "claude")).toBe(1);
  });

  /// Scenario: An agent started on the preview deck, which imitates a live one, renders in fixture mode; its type is printed once and nothing on its header or lease reads "Unavailable" or "unknown".
  it("previews a started agent the way a live tile shows it", () => {
    const started = createFixtureStartedAgent({ id: "9", daemonId: "deck-2001", displayName: "Desktop implementation", command: "codex", cwd: "/project" });
    const card = tile(started, "fixture");
    expect(within(card).getByRole("heading", { level: 2 })).toHaveTextContent(/^Desktop implementation$/);
    const header = card.querySelector("header")?.textContent ?? null;
    expect(occurrences(header, "codex")).toBe(1);
    expect(header).not.toMatch(/unavailable/i);
    expect(card.querySelector("footer")).not.toHaveTextContent(/LEASE|unknown/i);
  });

  /// Scenario: The preview deck's planner tile renders in fixture mode; it keeps its role, its model and its lease, which the preview has values for.
  it("keeps the preview's role, model and lease", () => {
    const planner = createFixtureSnapshot().agents.find((agent) => agent.id === "planner");
    if (!planner) throw new Error("the preview deck lists a planner");
    const card = tile(planner, "fixture");
    expect(within(card).getByRole("heading", { level: 2 })).toHaveTextContent(planner.displayName);
    expect(card.querySelector("header")).toHaveTextContent(planner.role);
    expect(card.querySelector("header")).toHaveTextContent(planner.model);
    expect(card.querySelector("footer")).toHaveTextContent(`LEASE · ${planner.writeLease.toUpperCase()}`);
  });
});
