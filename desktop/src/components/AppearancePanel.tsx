/**
 * The Appearance section (PRD #743 M4), and the settings surface's first real
 * tenant (PRD #803 M4).
 *
 * It implements `SettingsPanelProps` and nothing else — the sheet does not know
 * this is about themes, and this file does not know how the document is stored.
 *
 * It is also the worked example of the density rule in
 * `docs/develop/desktop-gui.md`: **one setting is one row**, laid out as two
 * columns — a 132px label column, then the control immediately beside it. This
 * was three full-width cards carrying a sentence of hint each — 195px of a
 * ~700px panel for a single setting, against 27px as one row — and the hints
 * were close to tautological ("Always light, whatever this machine is set to."
 * restates *Light*). Whatever density this first panel establishes is the one
 * #741 and #802 will copy, so it establishes the tight one — and that now
 * includes the label's 13px sub-header size and the width of the column it
 * sits in, both of which are the document's, not this panel's.
 *
 * It is the worked example of that document's text rule too, by subtraction: it
 * used to carry a paragraph explaining *why* the agent terminals stay dark in
 * both appearances. That is an engineering constraint, it is true, and it
 * changed nothing a reader does next — so it now lives only in
 * `docs/develop/desktop-gui.md` and `prds/743-desktop-light-dark-appearance.md`.
 * The failed-save alert below stays, because it is a consequence the user has to
 * act on.
 *
 * And it is the worked example of that document's heading rule, also by
 * subtraction: this panel opened with a `.form-heading` carrying an `APPEARANCE`
 * eyebrow over a `Light and dark` title, above a row whose own legend reads
 * *Appearance*. That put the word on screen twice and restated the three option
 * labels in the title, for 70px — more than the 61px the setting itself
 * occupies. A section heading is chrome for telling sections apart, and there is
 * one section.
 */
import { AlertTriangle } from "lucide-react";
import type { AppearanceMode } from "../lib/bridge";
import type { SettingsPanelProps } from "../lib/settingsContract";

const CHOICES: { value: AppearanceMode; label: string }[] = [
  { value: "system", label: "System" },
  { value: "light", label: "Light" },
  { value: "dark", label: "Dark" },
];

export function AppearancePanel({ settings, onSave, saveError }: SettingsPanelProps) {
  const current = settings.appearance.mode;

  return (
    <div className="settings-body">
      {/* A segmented control, not three buttons: still a real radio group, so
          one `name` buys arrow-key navigation and a single tab stop for free,
          and the legend names the group for a screen reader without a visible
          heading having to do that job. The legend is also the row's visible
          label, sitting in the grid's first column — see the `float` note on
          `.settings-row > legend` in `styles.css` for why a legend can be a
          grid item at all, and it is why deleting the heading above it took no
          label with it. */}
      <fieldset className="settings-row">
        <legend>Appearance</legend>
        <div className="segmented">
          {CHOICES.map((choice) => (
            <label key={choice.value} className={choice.value === current ? "is-selected" : ""}>
              <input
                type="radio"
                name="appearance"
                value={choice.value}
                checked={choice.value === current}
                onChange={() => onSave({ ...settings, appearance: { ...settings.appearance, mode: choice.value } })}
              />
              <span>{choice.label}</span>
            </label>
          ))}
        </div>
      </fieldset>

      {saveError && (
        <p className="settings-error" role="alert">
          <AlertTriangle size={13} />
          <span>This appearance is applied, but saving it failed, so it will not survive a restart. {saveError}</span>
        </p>
      )}
    </div>
  );
}
