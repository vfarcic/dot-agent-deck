/**
 * What the appearance choice actually does (PRD #743).
 *
 * The boundary with PRD #803 runs right here: #803 owns the store, the sheet
 * and the section registry, and knows nothing about themes; this module and
 * `components/AppearancePanel.tsx` are #743's, and they are the only things
 * that know what "dark" means.
 */
import type { AppearanceMode } from "./bridge";

/**
 * Put the choice on the document root, where `styles.css`'s dark blocks read it.
 *
 * **System removes the attribute rather than writing a value.** There is no
 * `[data-theme="system"]` block and there deliberately never will be: the
 * `@media (prefers-color-scheme: dark)` rule is scoped to
 * `:root:not([data-theme="light"])`, so an absent attribute is exactly what
 * lets the OS decide, in both directions and live. Writing `"system"` would
 * match neither dark block and would pin the app to light for the one choice
 * that is supposed to follow the machine.
 */
export function applyAppearance(mode: AppearanceMode): void {
  if (typeof document === "undefined") return;
  const root = document.documentElement;
  if (mode === "system") root.removeAttribute("data-theme");
  else root.setAttribute("data-theme", mode);
}
