/**
 * The zoom keybindings, and what a zoom change does (PRD #744 M3/M4).
 *
 * Three things happen on a zoom change and each is rate-limited differently,
 * because each has a different cost:
 *
 * 1. **The webview is scaled** — one IPC, straight through, because that is the
 *    thing the user is waiting to see.
 * 2. **Every mounted terminal is re-fitted** — coalesced to one pass per frame,
 *    because `fit()` measures layout and a held key would otherwise force one
 *    reflow per pane per key repeat. From there the daemon resize rides the
 *    bridge's own existing per-agent coalescing, which this file does not touch.
 * 3. **The level is persisted** — coalesced on a trailing interval, because
 *    every write is a temp-file-plus-rename of `desktop.toml` and a held key
 *    would otherwise do one per key repeat.
 *
 * What is deliberately NOT here: applying the stored level at launch. That is
 * `run()`'s `setup` hook in `src-tauri/src/lib.rs`, so the window is already at
 * the right level before any of this code runs and the user never sees a jump
 * from 100%. The split, and the argument against it, is in
 * `prds/744-desktop-zoom-text-size.md`.
 *
 * # Two writers, one level
 *
 * The keybindings come through here. The Settings row does not — it is an
 * ordinary `SettingsPanelProps` panel, so its only channel is `onSave`, and
 * widening that contract for one feature is exactly what PRD #803 built the
 * registry to avoid. So this hook owns the level for the session AND adopts an
 * externally-changed stored level, which is what makes the panel work without
 * knowing this hook exists. The subtlety that costs is the echo guard on
 * `lastPersisted` — see the sync effect.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import type { DesktopSettingsState } from "./useDesktopSettings";
import { applyZoomIntent, clampZoom, zoomIntentFromKey, type ZoomIntent } from "../lib/zoom";
import { refitAllTerminals } from "../lib/terminalRegistry";
import type { DeckRuntimeState } from "../types";

/**
 * How long a burst of zoom changes is allowed to run before the level is
 * written to disk.
 *
 * The shape is **leading edge plus a trailing flush**, not a debounce: the first
 * change of a burst is written straight through, later changes inside the
 * window replace a pending value without moving the deadline, and the deadline
 * flushes whatever accumulated. So a key held for ten seconds writes roughly
 * twice per interval rather than once at the end. A pure debounce would write
 * nothing at all until the key was released, which loses the level entirely if
 * the app is killed mid-burst — and the leading write closes a startup race
 * besides, which `commit` explains.
 *
 * 400ms is chosen against the OS key-repeat rate rather than against the disk:
 * long enough that any plausible repeat rate collapses to a couple of writes a
 * second, short enough that a single deliberate keypress is durable almost
 * immediately. The nearest precedent in this app is `SNAPSHOT_COALESCE_INTERVAL`
 * (150ms, `src-tauri/src/lib.rs`), which is shorter because it throttles an
 * in-memory emit rather than a file write.
 */
export const ZOOM_PERSIST_INTERVAL_MS = 400;

export interface ZoomState {
  /** The level in force, always a ladder value. */
  level: number;
  /** Step or reset. What the keybindings call. */
  apply: (intent: ZoomIntent) => void;
  /** Jump straight to a level. */
  set: (level: number) => void;
  /**
   * False where the bridge cannot scale a webview — the browser preview. The
   * keys are not bound there and the Settings row disables itself, because a
   * browser already has its own zoom and a level that persists while doing
   * nothing is worse than one that says it is unavailable.
   */
  available: boolean;
}

export function useZoom(runtime: DeckRuntimeState, settings: DesktopSettingsState): ZoomState {
  const { mode, setZoom } = runtime;
  const available = mode === "live";

  const stored = clampZoom(settings.settings.zoom.level);
  const [level, setLevel] = useState(stored);

  // The listener is registered once, so it reads the live values through refs
  // rather than through the closure it was created in. Re-registering per
  // render would work, but it would churn a capture-phase binding on every
  // keystroke, and the correctness of that binding is the load-bearing property
  // of this whole file.
  const levelRef = useRef(level);
  const settingsRef = useRef(settings);
  const setZoomRef = useRef(setZoom);
  levelRef.current = level;
  settingsRef.current = settings;
  setZoomRef.current = setZoom;

  const persistTimer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const persistPending = useRef<number>(undefined);
  /**
   * The last level this hook asked to be written.
   *
   * The echo guard. The stored document trails this hook by up to one persist
   * interval, so mid-burst it holds an *older* level than the one on screen —
   * and a sync effect that adopted the stored value unconditionally would pull
   * the level backwards under the user's fingers while they were still holding
   * the key. Comparing against this ref is what distinguishes "the document
   * caught up with us" from "somebody else changed it", the somebody else being
   * the Settings row.
   */
  const lastPersisted = useRef(stored);

  const write = useCallback((next: number) => {
    lastPersisted.current = next;
    const current = settingsRef.current;
    // The whole document, with only this section replaced — a save must never
    // drop a section this build's UI has not loaded (`settingsContract.ts`).
    current.save({ ...current.settings, zoom: { level: next } });
  }, []);

  const flushPersist = useCallback(() => {
    persistTimer.current = undefined;
    const pending = persistPending.current;
    persistPending.current = undefined;
    if (pending !== undefined) write(pending);
  }, [write]);

  const commit = useCallback((next: number) => {
    if (next === levelRef.current) return;
    levelRef.current = next;
    setLevel(next);
    if (persistTimer.current === undefined) {
      // **Leading edge**, and it is not only about matching the coalescer this
      // app already documents (`SnapshotCoalescer`: the first change writes
      // immediately, the rest are throttled, one trailing write flushes the
      // remainder). It closes a startup race that a purely trailing flush
      // leaves open.
      //
      // `useDesktopSettings` protects a user's choice from the asynchronous
      // initial load with an `edited` ref — but that ref is only set *inside*
      // `save`. With a trailing-only flush, `save` has not been called yet when
      // `getSettings()` resolves, so a zoom keystroke made in that window was
      // overwritten by whatever was on disk at launch: the change appeared to
      // take and then reverted, which is the exact failure that hook's own
      // comment describes. Writing the first change of a burst straight through
      // sets `edited` in time, and costs one extra write per burst.
      write(next);
      persistTimer.current = setTimeout(flushPersist, ZOOM_PERSIST_INTERVAL_MS);
    } else {
      persistPending.current = next;
    }
  }, [flushPersist, write]);

  const apply = useCallback((intent: ZoomIntent) => {
    commit(applyZoomIntent(levelRef.current, intent));
  }, [commit]);

  const set = useCallback((next: number) => {
    commit(clampZoom(next));
  }, [commit]);

  // Adopt a stored level this hook did not write — the async initial load, and
  // the Settings row, whose only channel is the document. An echo of our own
  // pending write is ignored, per `lastPersisted` above.
  useEffect(() => {
    if (stored === lastPersisted.current) return;
    // **An external change supersedes anything still queued here**, and
    // dropping it is not tidiness — keeping it is a bug with a two-part
    // symptom (Greptile P1 on PR #880). Hold a zoom key, then pick a level in
    // Settings: without this, the adopt below takes the new level while the
    // armed timer still holds the older keyboard one, and 400ms later that
    // stale value is written over the user's choice. The window then follows
    // it, because the write moves `lastPersisted` and the next render adopts
    // backwards — so the level visibly falls back a moment after being set,
    // and the wrong one is restored at the next launch.
    //
    // Nothing needs writing in its place: whoever changed the document has
    // already persisted it.
    if (persistTimer.current !== undefined) {
      clearTimeout(persistTimer.current);
      persistTimer.current = undefined;
    }
    persistPending.current = undefined;
    lastPersisted.current = stored;
    levelRef.current = stored;
    setLevel(stored);
  }, [stored]);

  /**
   * The one place a level is applied, keyed on the level itself — the shape PRD
   * #743's appearance effect uses, and for the same reason: two paths that both
   * apply can disagree, and one effect keyed on the value cannot disagree with
   * itself.
   *
   * **The first value seen is recorded, not applied**, and that is the entire
   * reason this hook does not fight the Rust launch apply. `settings` starts at
   * the defaults and resolves asynchronously, so the first level here is 100%
   * whatever the document says; applying it would slam a user stored at 150%
   * back to 100% and then jump them forward when the load landed — precisely
   * the flash the Rust leg exists to prevent. Once the load resolves, the
   * change to the stored level does reach `setZoom`, which is idempotent
   * against what Rust already did.
   */
  const applied = useRef<number>(undefined);
  useEffect(() => {
    if (applied.current === undefined) {
      applied.current = level;
      return;
    }
    if (applied.current === level) return;
    applied.current = level;
    // A rejected apply is swallowed rather than surfaced: the failure is
    // self-evident (nothing got bigger), and an error toast on a keystroke is
    // worse than none.
    void setZoomRef.current(level).catch(() => undefined);
    // The re-fit is what tells the daemon the PTY changed shape. Requested
    // unconditionally rather than left to each pane's own `ResizeObserver`: the
    // observer does fire under page zoom, but only because the pane's WIDTH
    // descends from the viewport — `.agent-panel`'s height is clamped at 320px
    // and stops moving above 110% — and none of it is observable in a test
    // environment with no layout engine.
    refitAllTerminals();
  }, [level]);

  useEffect(() => {
    if (!available) return;
    const onKeyDown = (event: KeyboardEvent) => {
      const intent = zoomIntentFromKey(event);
      if (!intent) return;
      // `preventDefault` alone is not enough, and this is the correctness
      // requirement the whole hook is arranged around.
      //
      // xterm binds its own `keydown` on `.xterm-helper-textarea` — focused
      // whenever a pane has focus, which is most of the time — and its key
      // evaluation maps `Ctrl` plus a single-character key with `keyCode >= 48`
      // to that literal character, and `Ctrl _` to 0x1F. `-` is 189, `=` is 187
      // and `0` is 48, so in the BUBBLE phase this listener would run after
      // xterm had already written a byte to the focused agent's PTY, and
      // `preventDefault` cannot retract a sent byte.
      //
      // A capture-phase listener on `window` runs before any listener on a
      // descendant, so `stopPropagation` here means xterm never sees the event
      // at all. Note the failure this closes is platform-split and would have
      // shipped: the macOS binding uses `metaKey`, which xterm's branch
      // excludes, so only Linux and Windows would have typed into the terminal.
      //
      // "claims the keys before a descendant listener can see them" in the test
      // file pins the `true` below; deleting it or the `stopPropagation`
      // re-opens the bug silently.
      event.preventDefault();
      event.stopPropagation();
      apply(intent);
    };
    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  }, [apply, available]);

  /**
   * Flush a pending level on every teardown path we can actually observe.
   *
   * The React unmount alone is not enough, and assuming it was is the second
   * defect Greptile caught on PR #880: closing the native window tears the
   * webview down without unmounting anything, so a burst whose trailing value
   * had not been written lost it. `pagehide` and a `visibilitychange` to
   * hidden are what the webview does give us, and they also cover the ordinary
   * "alt-tab away and come back later" case.
   *
   * **This is best effort and the residual gap is stated rather than papered
   * over.** `save` starts an asynchronous IPC that shutdown does not await, so
   * a hard kill inside that round trip can still lose the last change of a
   * burst. Two things bound it. The leading-edge write means a single
   * deliberate keypress is already on its way to disk before any of this
   * matters, so what remains at risk is the tail of a held-key burst.
   * And the alternative — persisting Rust-side inside `desktop_set_zoom` —
   * was rejected: it would make the keys and the Settings row write through
   * different code, losing the property that they cannot disagree, and it
   * would turn every keystroke into a read-modify-write of the whole document,
   * which is more disk traffic rather than less. A durable close would need
   * Rust-side close interception that awaits a webview round trip, which is a
   * disproportionate amount of machinery for a zoom level.
   */
  useEffect(() => {
    const flushNow = () => {
      if (persistTimer.current === undefined) return;
      clearTimeout(persistTimer.current);
      persistTimer.current = undefined;
      flushPersist();
    };
    const onHidden = () => {
      if (document.visibilityState === "hidden") flushNow();
    };
    window.addEventListener("pagehide", flushNow);
    document.addEventListener("visibilitychange", onHidden);
    return () => {
      window.removeEventListener("pagehide", flushNow);
      document.removeEventListener("visibilitychange", onHidden);
      flushNow();
    };
  }, [flushPersist]);

  return { level, apply, set, available };
}
