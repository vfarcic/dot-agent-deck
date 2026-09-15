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
 * # The row is a span plus `aria-labelledby`, and it used to be a fieldset
 *
 * It built the two columns by floating a `<legend>`, which stops the legend
 * being the fieldset's *rendered* legend and lets it be an ordinary box among
 * the fieldset's contents — the grid item in column one. Chromium honours the
 * float; **WebKit forces a rendered legend's `float` back to `none`**, so under
 * WebKit the legend never became a grid item, the segmented control landed in
 * column one beside its label, and the 132px column was wasted. Measured on the
 * browser tier at the same `?fixture=1` page: `getComputedStyle(legend).float`
 * was `"left"` under `chromium` and `"none"` under `webkit`, and the control's
 * x moved 148px ([#1032](https://github.com/vfarcic/dot-agent-deck/issues/1032),
 * found by PRD #741 M7). The app ships on WebKit — WebKitGTK under Tauri on
 * Linux, WKWebView on macOS — so the form that failed was the one that failed
 * where the users are, and the form that worked was the one only Chromium saw.
 *
 * `<span class="settings-row-label" id>` plus `role="radiogroup"
 * aria-labelledby` lays out identically in both engines and keeps the group's
 * accessible name, which `App.test.tsx` reaches by role. It is the form
 * `EndpointsPanel` and `DeckSelector` already use, and `e2e/settings-rows.spec.ts`
 * now measures it in every panel in both engines, so the next panel to float a
 * legend fails a test rather than shipping a collapsed row.
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
 * eyebrow over a `Light and dark` title, above a row whose own label reads
 * *Appearance*. That put the word on screen twice and restated the three option
 * labels in the title, for 70px — more than the 61px the setting itself
 * occupies. A section heading is chrome for telling sections apart, and there is
 * one section.
 */
import { useId } from "react";
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
  // `useId` rather than a constant, as in `DeckSelector`: the label is reached
  // by `aria-labelledby`, and two panels in one document — a test rendering two
  // shells — must not share an id.
  const labelId = useId();

  return (
    <div className="settings-body">
      {/* A segmented control, not three buttons: still a real radio group, so
          one `name` buys arrow-key navigation and a single tab stop for free,
          and `aria-labelledby` names the group for a screen reader without a
          visible heading having to do that job. The span is also the row's
          visible label, sitting in the grid's first column — see the WebKit
          note at the top of this file, and `.settings-row-label` in
          `styles.css`, for why it is a span rather than a legend. It is why
          deleting the heading above it took no label with it. */}
      <div className="settings-row">
        <span className="settings-row-label" id={labelId}>Appearance</span>
        <div className="segmented" role="radiogroup" aria-labelledby={labelId}>
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
      </div>

      {/* Rendered verbatim: `saveError` is a complete sentence composed by
          `useDesktopSettings`, because the same prop also carries "your
          settings file cannot be read" (issue #1072) — which must not acquire a
          "saving it failed" preamble it has not earned. */}
      {saveError && (
        <p className="settings-error" role="alert">
          <AlertTriangle size={13} />
          <span>{saveError}</span>
        </p>
      )}
    </div>
  );
}
