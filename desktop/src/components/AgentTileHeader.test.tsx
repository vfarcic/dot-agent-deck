import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, createFixtureStartedAgent } from "../data/fixture";
import { mapDesktopSnapshot, type DesktopSnapshotDto } from "../lib/bridge";
import { DISPLAY_LIMITS, displayIdentity, displayTitle } from "../lib/displayText";
import type { AgentSession, RuntimeMode } from "../types";
import { AgentTile } from "./AgentTile";

vi.mock("./TerminalViewport", () => ({ TerminalViewport: () => <pre>terminal</pre> }));

// Issue #2001: the tile's header names the agent, says its type once, and
// prints nothing for a value the deck does not have. Issue #1798: on one line,
// with an orchestration agent's run named before it. The live records come
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

function tile(agent: AgentSession, mode: RuntimeMode = "live", voiceNumber?: number) {
  render(<AgentTile agent={agent} presentation="tile" showDetails={false} mode={mode} voiceNumber={voiceNumber}
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

  /// Scenario: An orchestration's coder role is on the deck under its default name; the title reads "team / coder", naming its orchestration, and the role is not printed a second time.
  it("titles an orchestration role with its orchestration and its name, once", () => {
    const card = tile(liveAgent({
      displayName: "coder",
      tab: { kind: "orchestration", name: "team", roleIndex: 1, roleName: "coder", isStartRole: false },
    }));
    expect(within(card).getByRole("heading", { level: 2 })).toHaveTextContent(/^team \/ coder$/);
    expect(occurrences(card.querySelector("header")?.textContent ?? null, "coder")).toBe(1);
    expect(occurrences(card.querySelector("header")?.textContent ?? null, "claude")).toBe(1);
  });

  /// Scenario: An orchestration run started with the name "prd-1401" has its coder on the deck; the title names the run as the overview's group heading does, "prd-1401 / coder", not the orchestration's definition name.
  it("names the orchestration by its run's title when it has one", () => {
    const card = tile(liveAgent({
      displayName: "coder",
      tab: { kind: "orchestration", name: "team", displayTitle: "prd-1401", roleIndex: 1, roleName: "coder", isStartRole: false },
    }));
    expect(within(card).getByRole("heading", { level: 2 })).toHaveTextContent(/^prd-1401 \/ coder$/);
    expect(card.querySelector("header")).not.toHaveTextContent("team");
  });

  /// Scenario: An orchestration's coder role the user renamed "lead" is on the deck; the title reads "team / lead" and the same line still says it is the Coder role, running claude.
  it("keeps a renamed role's role name beside its new name", () => {
    const card = tile(liveAgent({
      displayName: "lead",
      tab: { kind: "orchestration", name: "team", roleIndex: 1, roleName: "coder", isStartRole: false },
    }));
    expect(within(card).getByRole("heading", { level: 2 })).toHaveTextContent(/^team \/ lead$/);
    const header = card.querySelector("header")?.textContent ?? null;
    expect(occurrences(header, "coder")).toBe(1);
    expect(occurrences(header, "claude")).toBe(1);
  });

  /// Scenario: A live agent the user named "dot-agent-deck" is on the deck with its pull request and inbox badge; its name, its binary, its status and the rename pencil are all on the title's one line, and no second identity line is left under it.
  it("puts the name, the binary and the rename pencil on one line", () => {
    const card = tile(liveAgent({ displayName: "dot-agent-deck" }));
    const line = card.querySelector(".agent-title-line");
    if (!line) throw new Error("the tile has a title line");
    expect(within(line as HTMLElement).getByRole("heading", { level: 2 })).toHaveTextContent(/^dot-agent-deck$/);
    expect(line).toHaveTextContent("claude");
    expect(line).toHaveTextContent(/running/i);
    expect(within(line as HTMLElement).getByRole("button", { name: "Rename dot-agent-deck" })).toBeVisible();
    expect(card.querySelector(".agent-identity > div > p:not([class])")).toBeNull();
  });

  /// Scenario: Press the pencil on the title line of an agent named "dot-agent-deck", type "reviewer" and press Enter; the deck is asked to rename that agent.
  it("still renames from the pencil on the title line", () => {
    const onRename = vi.fn(async () => {});
    render(<AgentTile agent={liveAgent({ displayName: "dot-agent-deck" })} presentation="tile" showDetails={false} mode="live"
      selected={false} tab="terminal" evidence={[]} onSelect={vi.fn()}
      onTabChange={vi.fn()} onTerminalInput={vi.fn(async () => {})}
      onTerminalResize={vi.fn(async () => {})} onEvidenceSelect={vi.fn()} onRename={onRename} />);
    fireEvent.click(screen.getByRole("button", { name: "Rename dot-agent-deck" }));
    const field = screen.getByRole("textbox", { name: "Rename dot-agent-deck" });
    fireEvent.change(field, { target: { value: "reviewer" } });
    fireEvent.keyDown(field, { key: "Enter" });
    expect(onRename).toHaveBeenCalledExactlyOnceWith("7", "reviewer", "header-process-a");
  });

  /// Scenario: An orchestration's coder role is on the deck with voice number 3; a screen reader names its tile "3. team / coder" and its controls after "team / coder", the way its title reads, not after its role alone.
  it("names the tile and its controls the way the title reads", () => {
    const card = tile(liveAgent({
      displayName: "coder",
      tab: { kind: "orchestration", name: "team", roleIndex: 1, roleName: "coder", isStartRole: false },
    }), "live", 3);
    expect(card).toHaveAccessibleName("3. team / coder");
    expect(within(card).getByRole("button", { name: "Rename team / coder" })).toBeVisible();
    expect(within(card).getByRole("tablist")).toHaveAccessibleName("team / coder details");
  });

  /// Scenario: A live agent whose name is 300 characters long is on the deck; its title is clamped the way the overview clamps names, and the whole name stays in the title's tooltip.
  it("clamps a long name and keeps the whole of it in the tooltip", () => {
    const long = "n".repeat(300);
    const card = tile(liveAgent({ displayName: long }));
    const heading = within(card).getByRole("heading", { level: 2 });
    expect(heading.textContent).toBe(displayIdentity(long, DISPLAY_LIMITS.name, "unnamed agent 7"));
    expect(heading.textContent?.length).toBeLessThan(long.length);
    expect(heading).toHaveAttribute("title", displayTitle(long));
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
