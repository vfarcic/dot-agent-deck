/**
 * The settings surface (PRD #803 M3): the sixth overlay and the footer line
 * that answers "where did that go?".
 *
 * A pure rendering component. The section registry it renders lives in
 * `lib/settingsRegistry.ts`, so a feature adding a section never opens this
 * file — which is what makes the contract in `lib/settingsContract.ts` true
 * rather than aspirational.
 *
 * A `config-sheet` rather than a screen, matching the four panels in
 * `ConfigurationPanels.tsx`. The app has no navigation model on `main` — one
 * always-mounted deck with five overlay booleans over it — and the sheet is
 * what a user of this app already recognises. It also keeps this independent of
 * PR #779's landing, which introduces the first `DeckView` union; if that turns
 * the rail into real navigation, Settings becomes a view and the panel
 * components move unchanged.
 *
 * **The section column appears only once there are at least two sections.** A
 * 232px list holding one entry beside ~700px of panel reads as unfinished work
 * rather than as a deliberate scope boundary, and the alternative — a footnote
 * naming what will land there later — would bake an opinion about the
 * container's future contents into the container, which is the specific thing
 * PRD #803 exists to avoid. Collapsing bakes in nothing: the registry still
 * drives the layout, so the column returns by itself the moment #741 or #802
 * adds a row. `SettingsSheet.test.tsx` pins both directions with stub sections,
 * so the column is provably real rather than asserted in a comment.
 */
import { useState } from "react";
import { FileCog, X } from "lucide-react";
import type { DesktopSettingsDto } from "../lib/bridge";
import type { SettingsSection } from "../lib/settingsContract";
import { SETTINGS_SECTIONS } from "../lib/settingsRegistry";
import type { RuntimeMode } from "../types";

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
  /**
   * The registry to render. Defaults to `SETTINGS_SECTIONS` from
   * `lib/settingsRegistry.ts`; the app never passes it. It exists so the
   * two-section layout is testable while the real registry holds one row — the
   * collapse below is otherwise a claim no test can reach until #741 or #802
   * lands.
   */
  sections?: SettingsSection[];
}

export function SettingsSheet({ open, onClose, settings, onSave, saveError, path, loaded, mode, sections = SETTINGS_SECTIONS }: SettingsSheetProps) {
  const [activeId, setActiveId] = useState(sections[0]?.id ?? "");
  if (!open) return null;

  const active = sections.find((section) => section.id === activeId) ?? sections[0];
  const Panel = active?.component;
  // One section is a full-width panel with no column to choose from; two or
  // more is a list beside a panel. See the note at the top of this file.
  const withColumn = sections.length > 1;

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
          </div>
          <button className="icon-button" aria-label="Close settings" onClick={onClose}><X size={18} /></button>
        </header>

        <div className={withColumn ? "settings-layout" : "settings-layout is-single"} data-testid="settings-layout">
          {withColumn && (
            <aside className="settings-sections">
              <nav aria-label="Settings sections">
                {sections.map((section) => {
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
          )}

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
    return <span data-testid="settings-location">Browser preview — kept in this browser's local storage, not in a file.</span>;
  }
  if (path) {
    return <span data-testid="settings-location">Stored in <code>{path}</code></span>;
  }
  return <span data-testid="settings-location">{loaded ? "Settings file location unavailable." : "Locating the settings file…"}</span>;
}
