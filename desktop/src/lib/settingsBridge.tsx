/**
 * The one bridge call a settings panel may make, handed down by context rather
 * than by a prop (PRD #741 M10).
 *
 * `SettingsPanelProps` is deliberately four props and says so: the document,
 * the save, the last error, and the runtime mode — the last of which was added
 * as the *general* environmental fact rather than a feature-specific one,
 * because "a prop only one panel could ever use would belong somewhere else".
 * A `testEndpoint` prop would have been exactly that, and #802 did in fact want
 * three more at its M4 — which arrived here rather than as props five, six and
 * seven, exactly as this file predicted.
 *
 * So the panel contract is unchanged and this is the escape hatch beside it,
 * with a deliberately small shape:
 *
 * - **Optional by construction.** `useSettingsBridge()` returns `undefined`
 *   wherever no provider is mounted — a panel rendered standalone in a test, a
 *   preview, a future surface that has no bridge — and a panel must render
 *   without it rather than throw. `EndpointsPanel` hides its Test connection
 *   button in that case, which is the honest thing: there is nothing to press.
 * - **Actions only, never state.** Nothing here holds a document, a snapshot or
 *   anything a render depends on. The settings document still travels the one
 *   way `SettingsPanelProps` describes, so this cannot become a second source
 *   of truth for what is stored.
 *
 * The provider is mounted by the component that already holds the runtime, so
 * the bridge reaches a panel without `SettingsSheet` — a #803-owned rendering
 * component — learning that endpoints or credentials exist.
 */
import { createContext, useContext } from "react";
import type { DesktopSettingsDto, EndpointTestReportDto, SecretStatusDto, VoiceSecretId } from "./bridge";

export interface SettingsBridge {
  /**
   * Test one deck end to end and resolve with a named state (PRD #741 M10).
   *
   * Takes the document because the row a user is testing is usually one they
   * have just typed; the Rust side reads no file for it.
   */
  testEndpoint: (settings: DesktopSettingsDto, selection: string) => Promise<EndpointTestReportDto>;
  /**
   * The three credential actions (PRD #802 M4), and the reason they are here
   * rather than on `SettingsPanelProps`.
   *
   * They are **actions, not state** — the rule this context was created with.
   * A credential is deliberately not part of the settings document, so it
   * cannot travel the one way the document travels; the panel asks the store
   * what it holds and tells it what to hold.
   *
   * There is no `loadSecret`, and adding one would defeat the seam: PRD #803's
   * rule is that a secret goes in neither `desktop.toml` nor `localStorage`,
   * and a value reaching the webview is one `JSON.stringify` from the second
   * half of that. `storeSecret` and `forgetSecret` **reject** when the store
   * failed, so a panel cannot render a failure as a saved key.
   */
  secretStatus: (id: VoiceSecretId) => Promise<SecretStatusDto>;
  storeSecret: (id: VoiceSecretId, secret: string) => Promise<SecretStatusDto>;
  forgetSecret: (id: VoiceSecretId) => Promise<SecretStatusDto>;
}

const SettingsBridgeContext = createContext<SettingsBridge | undefined>(undefined);

export const SettingsBridgeProvider = SettingsBridgeContext.Provider;

/** The bridge, or `undefined` where none is mounted. Never throws. */
export function useSettingsBridge(): SettingsBridge | undefined {
  return useContext(SettingsBridgeContext);
}
