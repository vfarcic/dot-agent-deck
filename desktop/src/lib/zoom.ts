/**
 * The zoom model: the ladder, how a keystroke maps to an intent, and nothing
 * else (PRD #744 M1).
 *
 * Deliberately pure — no DOM, no bridge, no React. What a zoom level *does*
 * lives in `hooks/useZoom.ts` (apply, re-fit, persist) and in
 * `src-tauri/src/lib.rs` (`webview.set_zoom`); this file only decides what the
 * next level is. That split is what makes the awkward half — layout-dependent
 * key matching — cheap to test in a jsdom suite that has no layout at all.
 */

/**
 * The levels the app steps through, ascending. Browser-familiar values, so the
 * feel matches every other app on the machine.
 *
 * **Both ends are measured rather than chosen.**
 *
 * The 3.0 ceiling is set by *width*, and by clipping rather than by taste:
 * `body` is `overflow-x: hidden` (`styles.css`), so horizontal overflow is
 * clipped while vertical overflow merely scrolls — width binds, height does
 * not. `html, body, #root` declare `min-width: 320px` and `tauri.conf.json`
 * declares `minWidth: 1024`, and page zoom divides the CSS-pixel viewport by
 * the level: 1024 / 3.0 = 341 is still clear of the floor, 1024 / 3.2 = 320
 * reaches it exactly. 3.0 is the last step that cannot clip at the smallest
 * window the app allows.
 *
 * The 0.75 floor is set by how small the type already is: the agent footer is
 * 6.8px, which renders at 5.1px here and 4.6px one browser step lower. Zooming
 * out past the point where the chrome is unreadable is not a feature.
 */
export const ZOOM_LEVELS: readonly number[] = [0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0];

/** What `Cmd 0` returns to, and what a document with no stored level reads as. */
export const DEFAULT_ZOOM = 1.0;

/** What a keystroke asked for. `undefined` means "not a zoom key". */
export type ZoomIntent = "in" | "out" | "reset";

/**
 * The nearest ladder level to `value`, or the default for anything that is not
 * a usable number.
 *
 * Snapping rather than rejecting, because the level is also a hand-editable
 * field in `desktop.toml`: a document written by a future build with a longer
 * ladder, or by a person typing `1.3`, should land somewhere sensible instead
 * of throwing the whole document away. This mirrors
 * `AppearanceMode::from_str_lossy`, which folds an unknown appearance token to
 * the default for the same reason, and the Rust side of this field clamps
 * identically so the two cannot disagree about what is stored.
 *
 * `Number.isFinite` is the guard rather than a range check because it is the
 * one test that rejects `NaN`, `Infinity` and `-Infinity` together — and `NaN`
 * is the case that matters, since every comparison against it is false and an
 * unguarded nearest-value search would return the first level rather than the
 * default.
 */
export function clampZoom(value: unknown): number {
  if (typeof value !== "number" || !Number.isFinite(value)) return DEFAULT_ZOOM;
  let nearest = ZOOM_LEVELS[0];
  for (const level of ZOOM_LEVELS) {
    if (Math.abs(level - value) < Math.abs(nearest - value)) nearest = level;
  }
  return nearest;
}

/**
 * One step along the ladder, saturating at both ends.
 *
 * Saturating rather than wrapping: holding the key down at 300% must stay at
 * 300%, not jump back to 75%. The starting value is snapped first, so a level
 * that arrived from a hand-edited document steps from the nearest rung rather
 * than getting stuck off-ladder.
 */
export function stepZoom(from: number, direction: "in" | "out"): number {
  const current = clampZoom(from);
  const index = ZOOM_LEVELS.indexOf(current);
  const next = index + (direction === "in" ? 1 : -1);
  if (next < 0 || next >= ZOOM_LEVELS.length) return current;
  return ZOOM_LEVELS[next];
}

/** Apply an intent. Split from `stepZoom` so the reset case has one home. */
export function applyZoomIntent(from: number, intent: ZoomIntent): number {
  return intent === "reset" ? DEFAULT_ZOOM : stepZoom(from, intent);
}

/**
 * `1.25` → `"125%"`, for the Settings row and the `?` overlay.
 *
 * Snaps first, so an off-ladder stored value reads as the rung it will actually
 * behave as rather than as itself — a document saying `1.3` steps from 125%, so
 * showing "130%" would be the one number on screen that nothing else agrees
 * with. Every ladder value is a whole percent, so the rounding never fires for
 * a snapped input; it is there for the arithmetic, not for display.
 */
export function formatZoom(level: number): string {
  return `${Math.round(clampZoom(level) * 100)}%`;
}

/**
 * The subset of a `KeyboardEvent` this module reads.
 *
 * Narrow on purpose: it makes the matcher callable from a test with a plain
 * object, and it documents that **`target` is never consulted**. That last part
 * is not incidental — issue #826 is `App.tsx`'s handler throwing on a keydown
 * whose target is not an element, because it casts `event.target` to
 * `HTMLElement` and calls `.matches` on it. Nothing here can reproduce that.
 */
export interface ZoomKeyEvent {
  key: string;
  code?: string;
  metaKey?: boolean;
  ctrlKey?: boolean;
  altKey?: boolean;
  shiftKey?: boolean;
}

/**
 * `event.key` values that mean each intent.
 *
 * `+` and `_` are the shifted forms of `=` and `-` on a US layout, and are what
 * `event.key` reports when Shift is held — so both are accepted and Shift is
 * not itself part of the match. Accepting `+` also covers the numpad on the
 * layouts that report it as a key rather than only as a code.
 */
const KEYS_IN = new Set(["=", "+"]);
const KEYS_OUT = new Set(["-", "_"]);

/**
 * `event.code` values that mean each intent — **the numpad only**.
 *
 * The division of labour matters and is easy to get backwards. `event.key` is
 * the *layout-resolved character*, so matching it is what makes the binding
 * correct on a non-US keyboard: wherever a user can type `-`, `event.key` is
 * `"-"`, whatever physical key produced it and whatever modifiers that took.
 * Matching the character keys by `code` as well would do the opposite of
 * helping — `code: "Equal"` names the physical US `=` position, which on a
 * German layout carries `´`, so it would bind a key the user never associates
 * with zoom while adding nothing for the key they do.
 *
 * The numpad is the one place `key` is not enough: with NumLock off the numeric
 * keys report navigation names (`"End"`, `"Insert"`), and `NumpadAdd` /
 * `NumpadSubtract` are reported inconsistently enough across platforms to be
 * worth naming by position. Those positions are the same on every layout, which
 * is exactly why they are safe to match this way and the character keys are
 * not.
 */
const CODES_IN = new Set(["NumpadAdd"]);
const CODES_OUT = new Set(["NumpadSubtract"]);
const CODES_RESET = new Set(["Numpad0"]);

/**
 * What a keydown asked for, or `undefined` if it is not a zoom keystroke.
 *
 * **`Cmd` on macOS, `Ctrl` elsewhere is deliberately not distinguished** —
 * either modifier is accepted on every platform. Reading the platform to pick
 * one buys nothing (no other app binds `Ctrl -` on macOS or `Cmd -` on Linux,
 * so there is nothing to collide with) and costs a `navigator` sniff that
 * `lib/platform.ts` shows is never exact. `Alt` is excluded because
 * `Alt Cmd -` and friends belong to the OS on macOS.
 */
export function zoomIntentFromKey(event: ZoomKeyEvent): ZoomIntent | undefined {
  if (!(event.metaKey || event.ctrlKey)) return undefined;
  if (event.altKey) return undefined;
  if (KEYS_IN.has(event.key) || (event.code !== undefined && CODES_IN.has(event.code))) return "in";
  if (KEYS_OUT.has(event.key) || (event.code !== undefined && CODES_OUT.has(event.code))) return "out";
  if (event.key === "0" || (event.code !== undefined && CODES_RESET.has(event.code))) return "reset";
  return undefined;
}
