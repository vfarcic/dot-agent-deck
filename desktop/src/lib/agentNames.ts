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
import { DISPLAY_LIMITS, displayIdentity, displayText, displayTitle, rendersBlank } from "./displayText";

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

/**
 * Issue #1798 — what an agent's tile calls it, in its title and in every name
 * a screen reader hears for the tile and its controls: its name, after its
 * run's title and ` / ` for an orchestration agent ("prd-1401 / coder"), so a
 * tile opened full-window still says which run it belongs to. `shown` clamps
 * each part the way the overview clamps a name, with a visible stand-in for one
 * that renders blank; `full` is the whole of it, for the tooltip.
 */
export function agentTitle(agent: Pick<AgentSession, "id" | "displayName" | "tab">): { shown: string; full: string } {
  const id = displayText(agent.id, DISPLAY_LIMITS.toolName);
  const name = displayIdentity(agent.displayName, DISPLAY_LIMITS.name, rendersBlank(id) ? "unnamed agent" : `unnamed agent ${id}`);
  if (agent.tab.kind !== "orchestration") return { shown: name, full: displayTitle(agent.displayName) };
  const run = orchestrationTitle(agent.tab);
  return {
    shown: `${displayIdentity(run, DISPLAY_LIMITS.name, "unnamed orchestration")} / ${name}`,
    full: displayTitle(`${run} / ${agent.displayName}`),
  };
}
