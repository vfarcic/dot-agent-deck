import { LayoutGrid, LayoutList } from "lucide-react";

import type { OverviewMode } from "../lib/agentCards";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";

/**
 * PRD #2374 — the agent dashboard's Table | Cards switch. Rendered only while
 * the experimental flag shows the cards; each half dispatches through the
 * registry, so "show cards" by voice and a click are one action.
 */
export function OverviewModeSwitch({ mode, controls }: { mode: OverviewMode; controls: Pick<VoiceActionContext, "setOverviewMode"> }) {
  return (
    <div className="overview-mode-switch" role="radiogroup" aria-label="Dashboard view" data-testid="overview-mode-switch">
      <button
        type="button"
        role="radio"
        aria-checked={mode === "table"}
        className={mode === "table" ? "is-selected" : undefined}
        data-testid="overview-mode-table"
        onClick={() => VOICE_ACTIONS.setOverviewMode.run(controls, { overviewMode: "table" })}
      ><LayoutList size={13} aria-hidden="true" /><span>Table</span></button>
      <button
        type="button"
        role="radio"
        aria-checked={mode === "cards"}
        className={mode === "cards" ? "is-selected" : undefined}
        data-testid="overview-mode-cards"
        onClick={() => VOICE_ACTIONS.setOverviewMode.run(controls, { overviewMode: "cards" })}
      ><LayoutGrid size={13} aria-hidden="true" /><span>Cards</span></button>
    </div>
  );
}
