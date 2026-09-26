/**
 * The contract a feature implements to put a setting in the settings surface
 * (PRD #803).
 *
 * The whole of it, deliberately — with one escape hatch beside it, named at the
 * bottom of this comment. Adding a **setting** is a field on your
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
 * Secrets never travel through here, and since PRD #802 M4 there is somewhere
 * else for them to go: `useSettingsBridge()`'s `storeSecret`/`forgetSecret`,
 * which reach the OS keychain through `src-tauri/src/secrets.rs` and never put
 * a value in the document. A settings document may hold a non-secret
 * *reference* — which backend holds the key, or a boolean saying one is stored —
 * and nothing more.
 *
 * Two checks watch that and they establish different things. The Rust-side
 * key-name check is a **naming tripwire, not a security boundary**: it reads key
 * names in the serialised document and nothing else, so a field called
 * `endpoint` holding a token passes it. `xtask/linkage-check`'s
 * `desktop_settings_secrets` is the structural one — it pins the field TYPES the
 * Rust schema may use (`String` is deliberately absent), this side's DTO names,
 * and the `localStorage` key set. It is what went red when #802 added its
 * section, which is how the credential ended up in the keychain rather than in
 * the document.
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
   * and a stale reply cannot overwrite newer state. Across *processes* — two
   * app windows, or the app and a hand edit — only the fields this call changed
   * are written, so another writer's edit to a field you did not touch survives
   * (issue #828). Compute `next` from the `settings` you were handed: the
   * difference between the two is what the hook treats as your edit.
   */
  onSave: (next: DesktopSettingsDto) => void;
  /**
   * Why the settings document cannot be written right now, as a **complete
   * sentence** — render it verbatim rather than composing around it.
   *
   * Two conditions reach this one prop and a panel cannot tell them apart,
   * which is deliberate: the last save failed (the choice is still applied for
   * this session; what failed is persisting it), or the document on disk cannot
   * be read at all, in which case the app is on defaults and every save is
   * refused so the user's file survives (issue #1072). `useDesktopSettings`
   * composes the right sentence for each. A panel that prefixed its own
   * "saving failed" lead-in would be wrong half the time — which is what they
   * all did before #1072, back when only one of the two existed.
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

/*
 * **The escape hatch, for a panel that needs to CALL something** (PRD #741 M10).
 *
 * The four props above are still the whole of the data contract: the document
 * travels one way and one way only, and nothing else supplies it. What #741's
 * `Test connection` needed was not data but an *action* — a bridge call — and a
 * `testEndpoint` prop would have been precisely the feature-specific prop the
 * `mode` note above rules out, with #802 wanting a sixth behind it.
 *
 * So actions come from `lib/settingsBridge.tsx`'s context instead:
 * `useSettingsBridge()` returns `undefined` where no provider is mounted, so a
 * panel renders without one rather than throwing, and nothing in it holds state
 * a render depends on. Read that file before adding to it.
 */

/** One row of the section registry. */
export interface SettingsSection {
  /** Stable id; also the `data-testid` suffix of the rendered panel. */
  id: string;
  label: string;
  icon: ComponentType<{ size?: number }>;
  component: ComponentType<SettingsPanelProps>;
}
