import { BookMarked, Bot, FolderGit2, Keyboard, LayoutList, Network, Settings2, SquareTerminal } from "lucide-react";
import type { ConnectionView, DesktopFeatures } from "../types";
import type { RailScreen, ShellOverlayState } from "../hooks/useShellOverlays";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import logoUrl from "../assets/logo.svg";

/** What the rail's entries dispatch against — the registry members they need, and no more. */
export type RailContext = Pick<VoiceActionContext, "navigate" | "openOverlay" | "closeOverlays">;

/**
 * Issue #1197 — the app's ONE navigation rail.
 *
 * It used to be rendered twice, by the deck and by the overview, with different
 * entries: the overview's offered Deck and Overview alone, so selecting it made
 * Settings and the deck's panels vanish. It is now rendered once, by whichever
 * shell owns the screens, with the same entries on every screen; only which one
 * is current changes.
 *
 * `aria-current="page"` names the screen underneath — Overview or Deck — while
 * no overlay is open, and the overlay's own entry while one is. An agent pane
 * opened from the overview keeps Overview current, because the overview is what
 * stays mounted beneath it.
 *
 * Every entry still dispatches through the action registry (PRD #802 M2). An
 * entry for a deck panel pressed on the overview goes to the deck and opens it
 * there, since those panels exist only on the deck; Settings opens over
 * whichever screen is up.
 *
 * Issue #1198 — the deck and its four panels are experimental surfaces, so
 * each entry renders only while its own `features` field says so. With the
 * flag off — the shipped default — the rail is Overview and Settings. Nothing
 * else here reads the flag: the entries that remain dispatch exactly as they
 * did.
 */
export function NavigationRail({ screen, overlays, context, connection, features, onShowShortcuts }: { screen: RailScreen; overlays: ShellOverlayState; context: RailContext; connection: ConnectionView; features: DesktopFeatures; onShowShortcuts?: () => void }) {
  const overlayOpen = Boolean(overlays.projects || overlays.prompts || overlays.profiles || overlays.workflow || overlays.settings);
  /* Deck is what the deck's rail used to call Runs: on the deck it clears the
     overlays, as Runs always did, and from the overview it goes to the deck. */
  const toDeck = () => (screen === "deck" ? VOICE_ACTIONS.showRuns.run(context) : VOICE_ACTIONS.openDeck.run(context));
  return (
    <aside className="rail" aria-label="Primary navigation">
      {/* Issue #746: `assets/logo.svg` is a copy of assets/brand/logo.svg written by
          scripts/brand-icons.sh — edit the master and rerun that, never this copy. */}
      <img className="brand-mark" src={logoUrl} alt="Agent Deck" width={36} height={36} />
      <nav>
        <RailButton icon={LayoutList} label="Overview" active={screen === "overview" && !overlayOpen} onClick={() => VOICE_ACTIONS.openOverview.run(context)} testId="open-overview" />
        {features.showDeck && <RailButton icon={SquareTerminal} label="Deck" active={screen === "deck" && !overlayOpen} onClick={toDeck} testId="open-deck" />}
        {features.showProjects && <RailButton icon={FolderGit2} label="Projects" active={overlays.projects} onClick={() => VOICE_ACTIONS.openProjects.run(context)} testId="open-projects" />}
        {features.showPrompts && <RailButton icon={BookMarked} label="Prompts" active={overlays.prompts} onClick={() => VOICE_ACTIONS.openPromptLibrary.run(context)} testId="open-prompts" />}
        {features.showWorkflows && <RailButton icon={Network} label="Workflows" active={overlays.workflow} onClick={() => VOICE_ACTIONS.openWorkflowOrder.run(context)} />}
        {features.showAgentProfiles && <RailButton icon={Bot} label="Agent Profiles" active={overlays.profiles} onClick={() => VOICE_ACTIONS.openAgentProfiles.run(context)} testId="open-agent-profiles" />}
        <RailButton icon={Settings2} label="Settings" active={overlays.settings} onClick={() => VOICE_ACTIONS.openSettings.run(context)} testId="open-settings" />
      </nav>
      <div className="rail-bottom">
        {/* The sheet lists the deck's shortcuts, so it is offered where they work. */}
        {onShowShortcuts && <button aria-label="Keyboard shortcuts" title="Keyboard shortcuts" onClick={onShowShortcuts}><Keyboard size={18} /></button>}
        {/* PRD #741 final audit F5: `connection.message` is daemon-supplied —
            it embeds the REMOTE daemon's `build_version`, which is an
            unvalidated string on the wire — and `safe_message` on the Rust
            side covers category `Cc` only, so the bidi controls reach here
            intact. Same seam treatment as every other daemon string. */}
        <span className={`connection-lamp connection-${connection.status}`} title={connection.message ? displayText(connection.message, DISPLAY_LIMITS.message) : undefined} />
      </div>
    </aside>
  );
}

function RailButton({ icon: Icon, label, active, onClick, testId }: { icon: typeof SquareTerminal; label: string; active?: boolean; onClick: () => void; testId?: string }) {
  return <button className={active ? "is-active" : ""} aria-current={active ? "page" : undefined} title={label} onClick={onClick} data-testid={testId}><Icon size={18} /><span>{label}</span></button>;
}
