/**
 * The Appearance section (PRD #743 M4), and the settings surface's first real
 * tenant (PRD #803 M4).
 *
 * It implements `SettingsPanelProps` and nothing else — the sheet does not know
 * this is about themes, and this file does not know how the document is stored.
 */
import { AlertTriangle, SquareTerminal } from "lucide-react";
import type { AppearanceMode } from "../lib/bridge";
import type { SettingsPanelProps } from "../lib/settingsContract";

const CHOICES: { value: AppearanceMode; label: string; hint: string }[] = [
  { value: "system", label: "System", hint: "Follow this machine's light/dark setting, and keep following it when it changes." },
  { value: "light", label: "Light", hint: "Always light, whatever this machine is set to." },
  { value: "dark", label: "Dark", hint: "Always dark, whatever this machine is set to." },
];

export function AppearancePanel({ settings, onSave, saveError }: SettingsPanelProps) {
  const current = settings.appearance.mode;

  return (
    <>
      <div className="form-heading">
        <div><span>APPEARANCE</span><h3>Light and dark</h3></div>
      </div>
      <div className="settings-body">
        {/* A real radio group: one `name` gives arrow-key navigation and a
            single tab stop for free, and the legend names it for a screen
            reader without a visible heading having to do that job. */}
        <fieldset className="appearance-choices">
          <legend>Appearance</legend>
          {CHOICES.map((choice) => (
            <label key={choice.value} className={choice.value === current ? "is-selected" : ""}>
              <input
                type="radio"
                name="appearance"
                value={choice.value}
                checked={choice.value === current}
                onChange={() => onSave({ ...settings, appearance: { ...settings.appearance, mode: choice.value } })}
              />
              <span className="appearance-mark" aria-hidden="true" />
              <span className="appearance-copy"><strong>{choice.label}</strong><small>{choice.hint}</small></span>
            </label>
          ))}
        </fieldset>

        {saveError && (
          <p className="settings-error" role="alert">
            <AlertTriangle size={13} />
            <span>This appearance is applied, but saving it failed, so it will not survive a restart. {saveError}</span>
          </p>
        )}

        <p className="settings-note">
          <SquareTerminal size={13} />
          <span>
            The agent terminals stay dark in both appearances. Agent CLIs pick colours for a dark
            terminal — dim greys tuned to read on black, and truecolor output that ignores the
            palette entirely — so a light pane would be unreadable in a way Agent Deck cannot fix
            from here.
          </span>
        </p>
      </div>
    </>
  );
}
