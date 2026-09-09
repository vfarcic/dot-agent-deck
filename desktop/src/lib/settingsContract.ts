/**
 * The contract a feature implements to put a setting in the settings surface
 * (PRD #803).
 *
 * The whole of it, deliberately. Adding a **setting** is a field on your
 * feature's section struct in `src-tauri/src/settings.rs` plus an edit to your
 * own panel. Adding a **section** is one row in `SETTINGS_SECTIONS`
 * (`lib/settingsRegistry.ts`) and one component implementing
 * {@link SettingsPanelProps}. Neither requires touching the store, the sheet,
 * or anything belonging to another feature — the registry is a module of its
 * own precisely so that claim holds: while it lived inside the sheet, every
 * dependent had to edit a #803-owned rendering component to register, and two
 * of them would have collided on adjacent lines of the same array.
 *
 * There is no generic key/value renderer here, and that is a decision rather
 * than an omission: #741's endpoint list and #802's model manager are not
 * key/value widgets, and a renderer built to fit both would fit neither. A
 * panel is an ordinary React component and owns its own layout.
 *
 * Secrets never travel through here. A settings document may hold a non-secret
 * *reference* — which backend holds the key, or a boolean saying one is stored —
 * and nothing more. The Rust-side check that pins this is a **naming tripwire,
 * not a security boundary**: it reads key names in the serialised document and
 * nothing else, so a field called `endpoint` holding a token passes it, and
 * nothing on this side of the bridge is scanned at all. Issue #827 carries the
 * checks #802 needs before it stores a real key.
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
   *
   * Saves are serialised inside `useDesktopSettings` and a superseded response
   * is dropped, so two rapid calls reach the disk in the order they were made
   * and a stale reply cannot overwrite newer state. Two *processes* racing on
   * the same field is a different problem, not handled, tracked as #828.
   */
  onSave: (next: DesktopSettingsDto) => void;
  /**
   * Set when the last save failed. The choice is still applied for this
   * session; what failed is persisting it, and a panel should say so rather
   * than silently reverting.
   */
  saveError?: string;
  /**
   * Which runtime the app is in, so a panel can say what it cannot do here
   * (PRD #744).
   *
   * Added as a fourth prop rather than read from `selectRuntimeMode()` inside
   * whichever panel wants it, because the sheet already has it and a panel
   * should not be sniffing `window.location` in a render. It is deliberately
   * the *general* environmental fact rather than a feature-specific one — the
   * browser preview cannot scale a webview (#744), and it will not be able to
   * reach a daemon endpoint (#741) or a local model (#802) either, so all three
   * tenants need exactly this. A prop only one panel could ever use would
   * belong somewhere else.
   */
  mode: import("../types").RuntimeMode;
}

/** One row of the section registry. */
export interface SettingsSection {
  /** Stable id; also the `data-testid` suffix of the rendered panel. */
  id: string;
  label: string;
  icon: ComponentType<{ size?: number }>;
  component: ComponentType<SettingsPanelProps>;
}
