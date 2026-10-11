/**
 * Issue #1667 — how an orchestration and its agents are named, in one place
 * so the overview's group heading and what Reading says cannot drift apart.
 *
 * An orchestration is named by its RUN's title (the Name it was started
 * with), falling back to the orchestration's definition name when it was
 * started without one. The run title, not the definition name: dispatched
 * units all run the same definition, so the definition name would still
 * collide.
 *
 * An agent that is part of an orchestration is named by that title plus its
 * own name ("prd-1401 Coder"); a standalone agent by its own name. Two runs
 * with the same title, or two untitled runs of one definition, still share a
 * name — accepted in the issue, since dispatched units carry distinct titles.
 */

import type { AgentSession, AgentTab } from "../types";

/** The title an orchestration's group heading shows: the run's title, else the orchestration's name. */
export function orchestrationTitle(tab: Pick<Extract<AgentTab, { kind: "orchestration" }>, "displayTitle" | "name">): string {
  return tab.displayTitle || tab.name;
}

/**
 * What an agent is called where it is not on screen: the run's title and its
 * name for an orchestration agent, its name otherwise. With `maxChars`, at
 * most that many characters, cut from the run's title first so a long title
 * never cuts off the agent's own name, which is what tells a run's agents
 * apart.
 */
export function agentSpokenName(agent: Pick<AgentSession, "displayName" | "tab">, maxChars = Infinity): string {
  const name = Array.from(agent.displayName);
  if (name.length >= maxChars || agent.tab.kind !== "orchestration") return name.slice(0, maxChars).join("");
  const title = Array.from(orchestrationTitle(agent.tab)).slice(0, maxChars - name.length - 1).join("").trimEnd();
  return title === "" ? agent.displayName : `${title} ${agent.displayName}`;
}
