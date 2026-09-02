/**
 * The settings surface (PRD #803 M3): the sixth overlay, the section registry,
 * and the footer line that answers "where did that go?".
 *
 * A `config-sheet` rather than a screen, matching the four panels in
 * `ConfigurationPanels.tsx`. The app has no navigation model on `main` — one
 * always-mounted deck with five overlay booleans over it — and the sheet is
 * what a user of this app already recognises. It also keeps this independent of
 * PR #779's landing, which introduces the first `DeckView` union; if that turns
 * the rail into real navigation, Settings becomes a view and the panel
 * components move unchanged.
 */
import { useState } from "react";
import { FileCog, Palette, X } from "lucide-react";
import type { DesktopSettingsDto } from "../lib/bridge";
import type { SettingsSection } from "../lib/settingsContract";
import type { RuntimeMode } from "../types";
import { AppearancePanel } from "./AppearancePanel";

/**
 * The section registry. Adding a section is one row here and one component.
 *
 * It has exactly one entry, and that is the scope rather than an unfinished
 * state: #741's daemon endpoints and #802's voice backends each add their own
 * when they land. Pre-creating empty sections for them would be this container
 * growing opinions about its contents, which is the specific failure PRD #803
 * exists to prevent — a container with opinions blocks the dependents it was
 * built for.
 */
export const SETTINGS_SECTIONS: SettingsSection[] = [
  { id: "appearance", label: "Appearance", icon: Palette, component: AppearancePanel },
];

interface SettingsSheetProps {
  open: boolean;
  onClose: () => void;
  settings: DesktopSettingsDto;
  onSave: (next: DesktopSettingsDto) => void;
  saveError?: string;
  /** Where the document lives; absent in the browser preview and if the read failed. */
  path?: string;
  loaded: boolean;
  mode: RuntimeMode;
}

export function SettingsSheet({ open, onClose, settings, onSave, saveError, path, loaded, mode }: SettingsSheetProps) {
  const [activeId, setActiveId] = useState(SETTINGS_SECTIONS[0]?.id ?? "");
  if (!open) return null;

  const active = SETTINGS_SECTIONS.find((section) => section.id === activeId) ?? SETTINGS_SECTIONS[0];
  const Panel = active?.component;

  return (
    <div className="sheet-backdrop" role="presentation" onMouseDown={onClose}>
      <section
        className="config-sheet settings-sheet"
        role="dialog"
        aria-modal="true"
        aria-labelledby="settings-title"
        data-testid="settings-panel"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <header className="sheet-header">
          <div>
            <span className="eyebrow">APPLICATION SETTINGS</span>
            <h2 id="settings-title">Settings</h2>
            <p>Preferences for this installation of Agent Deck, on this machine. Everything else — the project, the agents, the run — comes from the daemon.</p>
          </div>
          <button className="icon-button" aria-label="Close settings" onClick={onClose}><X size={18} /></button>
        </header>

        <div className="settings-layout">
          <aside className="settings-sections">
            <nav aria-label="Settings sections">
              {SETTINGS_SECTIONS.map((section) => {
                const Icon = section.icon;
                const selected = section.id === active?.id;
                return (
                  <button
                    key={section.id}
                    className={selected ? "is-selected" : ""}
                    aria-current={selected ? "page" : undefined}
                    data-testid={`settings-section-${section.id}`}
                    onClick={() => setActiveId(section.id)}
                  >
                    <Icon size={15} />
                    <span>{section.label}</span>
                  </button>
                );
              })}
            </nav>
          </aside>

          <div className="settings-active" data-testid={active ? `settings-panel-${active.id}` : undefined}>
            {Panel ? <Panel settings={settings} onSave={onSave} saveError={saveError} /> : <div className="configuration-empty">No settings sections are registered.</div>}
          </div>
        </div>

        <footer className="sheet-footer settings-footer">
          <span>
            <FileCog size={13} />
            <SettingsLocation mode={mode} path={path} loaded={loaded} />
          </span>
        </footer>
      </section>
    </div>
  );
}

/**
 * Where the settings live, said honestly in each of the three cases.
 *
 * The browser preview genuinely has no file — `FixtureDeckBridge` keeps
 * settings in `localStorage` and structurally cannot reach the filesystem — so
 * it must not print a plausible-looking path for something that does not exist.
 */
function SettingsLocation({ mode, path, loaded }: { mode: RuntimeMode; path?: string; loaded: boolean }) {
  if (mode === "fixture") {
    return <span data-testid="settings-location">Browser preview — settings are kept in this browser's local storage, not in a file. The desktop app stores them on your machine.</span>;
  }
  if (path) {
    return <span data-testid="settings-location">Stored in <code>{path}</code> — readable, editable and deletable without Agent Deck running.</span>;
  }
  return <span data-testid="settings-location">{loaded ? "Settings file location unavailable." : "Locating the settings file…"}</span>;
}
