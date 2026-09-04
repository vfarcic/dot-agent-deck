/**
 * The Zoom section (PRD #744 M5), and the settings surface's second tenant.
 *
 * It implements `SettingsPanelProps` and nothing else. The sheet does not know
 * this is about scaling, and this file does not know how the document is
 * stored or how a level reaches the webview — writing the document *is* how a
 * level reaches the webview, because `useZoom` adopts an externally-changed
 * stored level. That indirection is deliberate: widening
 * `SettingsPanelProps` so one panel could reach one feature's hook is exactly
 * what PRD #803 built the registry to avoid.
 *
 * # Why this row exists at all, given the shortcut
 *
 * `Cmd +/-/0` needs no teaching. The row is not here to teach it — it is here
 * because **a webview zoom has no indicator of its own**. No other surface in
 * this app displays the level (the `?` overlay lists the keys, not the value),
 * and at 110% against 100% it is not a question you can answer by looking. The
 * hand-editable `desktop.toml` holds it, which is not the same as the app
 * telling you. Choosing `100%` here is also the reset, so there is no separate
 * reset control.
 *
 * # A select, not a segmented control
 *
 * `docs/develop/desktop-gui.md` picks the control by cardinality: a switch for
 * a boolean, a segmented control for up to about four exclusive options, a
 * select beyond that. There are ten levels, so it is a select — the segmented
 * control the Appearance panel uses would be 700px of buttons.
 */
import { AlertTriangle } from "lucide-react";
import type { SettingsPanelProps } from "../lib/settingsContract";
import { clampZoom, formatZoom, ZOOM_LEVELS } from "../lib/zoom";

export function ZoomPanel({ settings, onSave, saveError, mode }: SettingsPanelProps) {
  const current = clampZoom(settings.zoom.level);
  // `FixtureDeckBridge` cannot reach a webview, so there is nothing here to
  // scale. `useZoom` declines to bind the keys for the same reason.
  const available = mode === "live";

  return (
    <div className="settings-body">
      <div className="settings-row">
        <label htmlFor="zoom-level">Zoom</label>
        <select
          id="zoom-level"
          value={current}
          disabled={!available}
          // `clampZoom` is defensive rather than load-bearing here: every
          // option below is already a ladder value, so it can only ever be a
          // no-op through this control. It stays because the handler is what a
          // later control — a slider, a typed field — would reuse, and because
          // the level it produces is written straight to disk.
          onChange={(event) => onSave({ ...settings, zoom: { level: clampZoom(Number(event.target.value)) } })}
        >
          {ZOOM_LEVELS.map((level) => (
            <option key={level} value={level}>{formatZoom(level)}</option>
          ))}
        </select>
      </div>

      {/* The one piece of panel prose in the app, and it clears
          `docs/develop/desktop-gui.md`'s text bar on the stated exception —
          "an error, or a consequence the user has to act on". A browser preview
          cannot scale a webview, so the actionable half is telling the reader
          where the zoom they want actually is. It renders in fixture mode
          alone, which a packaged app is not. */}
      {!available && (
        <p className="settings-hint">Use your browser's own zoom in the preview; this scales the desktop window.</p>
      )}

      {saveError && (
        <p className="settings-error" role="alert">
          <AlertTriangle size={13} />
          <span>This zoom level is applied, but saving it failed, so it will not survive a restart. {saveError}</span>
        </p>
      )}
    </div>
  );
}
