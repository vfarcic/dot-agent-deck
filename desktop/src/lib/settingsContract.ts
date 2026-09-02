/**
 * The contract a feature implements to put a setting in the settings surface
 * (PRD #803).
 *
 * The whole of it, deliberately. Adding a **setting** is a field on your
 * feature's section struct in `src-tauri/src/settings.rs` plus an edit to your
 * own panel. Adding a **section** is one row in `SETTINGS_SECTIONS`
 * (`components/SettingsSheet.tsx`) and one component implementing
 * {@link SettingsPanelProps}. Neither requires touching the store, the sheet,
 * or anything belonging to another feature.
 *
 * There is no generic key/value renderer here, and that is a decision rather
 * than an omission: #741's endpoint list and #802's model manager are not
 * key/value widgets, and a renderer built to fit both would fit neither. A
 * panel is an ordinary React component and owns its own layout.
 *
 * Secrets never travel through here. A settings document may hold a non-secret
 * *reference* — which backend holds the key, or a boolean saying one is stored —
 * and nothing more; the Rust-side guard fails the build otherwise.
 */
import type { ComponentType } from "react";
import type { DesktopSettingsDto } from "./bridge";

/** What every settings panel is handed. */
export interface SettingsPanelProps {
  /** The whole document, so a panel can read a sibling section if it must. */
  settings: DesktopSettingsDto;
  /**
   * Persist a new document. Applied to the UI immediately and written behind
   * it, so a panel never has to manage a pending state of its own.
   *
   * Send the whole document — spread `settings` and replace your own section —
   * so a save can never drop a section this build's UI has not loaded.
   */
  onSave: (next: DesktopSettingsDto) => void;
  /**
   * Set when the last save failed. The choice is still applied for this
   * session; what failed is persisting it, and a panel should say so rather
   * than silently reverting.
   */
  saveError?: string;
}

/** One row of the section registry. */
export interface SettingsSection {
  /** Stable id; also the `data-testid` suffix of the rendered panel. */
  id: string;
  label: string;
  icon: ComponentType<{ size?: number }>;
  component: ComponentType<SettingsPanelProps>;
}
