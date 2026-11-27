import { useEffect } from "react";
import { BellRing, LayoutGrid, LayoutList } from "lucide-react";

import { fleetMockState, fleetMockView } from "../data/fleetMock";

import type { OverviewMode } from "../lib/agentCards";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";

/**
 * PRD #2374 — the agent dashboard's Table | Cards switch. Rendered while the
 * runtime offers cards; each half dispatches through the registry, so "show
 * cards" by voice and a click are one action.
 *
 * It is also where the app starts consuming cards: mounting it calls
 * `consumeCards` (the runtime's `consumeAgentCards`), and until something
 * does, no daemon is subscribed to or read for cards.
 */
export function OverviewModeSwitch({ mode, controls, consumeCards }: { mode: OverviewMode; controls: Pick<VoiceActionContext, "setOverviewMode">; consumeCards?: () => void }) {
  useEffect(() => {
    consumeCards?.();
  }, [consumeCards]);
  /* PRD 2557 MOCK-UP: the fleet view as a third view of the dashboard, shown only in the mock scene. */
  const mock = fleetMockState() !== undefined;
  const fleet = mock && fleetMockView() !== "cards";
  return (
    <div className="overview-mode-switch" role="radiogroup" aria-label="Dashboard view" data-testid="overview-mode-switch">
      <button
        type="button"
        role="radio"
        aria-checked={!fleet && mode === "table"}
        className={!fleet && mode === "table" ? "is-selected" : undefined}
        data-testid="overview-mode-table"
        aria-label="Table"
        title="Table"
        onClick={() => VOICE_ACTIONS.setOverviewMode.run(controls, { overviewMode: "table" })}
      ><LayoutList size={13} aria-hidden="true" /><span>Table</span></button>
      <button
        type="button"
        role="radio"
        aria-checked={!fleet && mode === "cards"}
        className={!fleet && mode === "cards" ? "is-selected" : undefined}
        data-testid="overview-mode-cards"
        aria-label="Cards"
        title="Cards"
        onClick={() => VOICE_ACTIONS.setOverviewMode.run(controls, { overviewMode: "cards" })}
      ><LayoutGrid size={13} aria-hidden="true" /><span>Cards</span></button>
      {mock && <button type="button" role="radio" aria-checked={fleet} className={`is-mock-fleet${fleet ? " is-selected" : ""}`} data-testid="overview-mode-fleet" aria-label="Fleet" title="Fleet: what needs you across the dispatcher's units"><BellRing size={13} aria-hidden="true" /><span>Fleet</span></button>}
    </div>
  );
}
