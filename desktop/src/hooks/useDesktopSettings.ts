/**
 * The desktop app's own settings, loaded once at startup and written through
 * the bridge (PRD #803).
 *
 * Deliberately knows nothing about what any setting *means* — it holds the
 * document, the path, and why the document cannot be written when it cannot,
 * and nothing else. What the appearance choice does is `lib/appearance.ts`'s,
 * and #741's and #802's sections will add fields here without this file
 * changing at all.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { DEFAULT_DESKTOP_SETTINGS, type DesktopSettingsDto } from "../lib/bridge";
import type { DeckRuntimeState } from "../types";

export interface DesktopSettingsState {
  settings: DesktopSettingsDto;
  /**
   * Where the document lives. Absent until the load resolves, in the browser
   * preview (which has no filesystem), and if the read failed.
   */
  path?: string;
  /** False until the first load resolves, so the UI can avoid claiming a value it has not read. */
  loaded: boolean;
  /**
   * Whether `settings` holds a document somebody actually has — read from disk,
   * or changed in this session — rather than the placeholder this hook seeds
   * itself with. False from mount until one of those two happens, which on a
   * failed read and no interaction is for the life of the hook.
   *
   * Distinct from `loaded`, and issue #845 is why. `loaded` answers "has the
   * read settled"; a consumer that writes to global chrome needs "is this mode
   * one somebody chose". They part company in both directions:
   *
   * - A choice made *while* the read is in flight is chosen but not loaded.
   *   Gating such a consumer on `loaded` would leave that click unapplied until
   *   the read lands — the no-restart requirement PRD #743 puts on the
   *   appearance choice, and the race the `edited` guard below exists for.
   * - A read that **failed** is loaded but chose nothing. `settings` then holds
   *   the same placeholder it was seeded with, and treating that as a choice
   *   would let a transient IPC failure overwrite the palette the user actually
   *   stored — which this app has already applied, from the same file, before
   *   this bundle ran.
   */
  chosen: boolean;
  /**
   * Why the settings document cannot be written right now, as a complete
   * sentence ready to render. `undefined` when there is nothing wrong.
   *
   * **Two things land here, and issue #1072 is why they share one slot.**
   *
   * - The last **save** failed. The change stays applied for this session; what
   *   failed is making it survive a restart, and the sentence says so.
   * - The document on **disk cannot be read** — a syntax error, or a value this
   *   build's schema rejects. The app is then running on defaults and the Rust
   *   side refuses every save rather than publishing those defaults over the
   *   user's file, so this is set from the moment the load resolves, before any
   *   save has been attempted. This is what explains why a user's settings
   *   look reset; the footer carries the same sentence as `problem` below, in
   *   place of the path it would otherwise name (issue #829).
   *
   * A save failure wins while both are set: it is the newer answer, and the
   * refusal message carries the same locator the load problem does.
   */
  saveError?: string;
  /**
   * Why the document at `path` is not in use, when it is not — the load
   * problem alone, without a save failure folded in. `undefined` once a save
   * succeeds, because that proves the path usable.
   *
   * Separate from `saveError` because the footer answers a different question
   * from the panel's alert: "where are my settings kept?" A failed save does
   * not change that answer; a document the app refuses to use does (issue
   * #829), and the footer must not go on naming that path as the store.
   */
  problem?: string;
  save: (next: DesktopSettingsDto) => void;
}

export function useDesktopSettings(runtime: DeckRuntimeState): DesktopSettingsState {
  const { getSettings, saveSettings } = runtime;
  const [settings, setSettings] = useState<DesktopSettingsDto>(DEFAULT_DESKTOP_SETTINGS);
  const [path, setPath] = useState<string>();
  const [loaded, setLoaded] = useState(false);
  // Whether a document actually came back, which `loaded` does NOT answer:
  // `loaded` is true once the read has SETTLED, failure included. The two part
  // company only on the failure path, and that is exactly where the difference
  // is load-bearing — see `chosen`.
  const [read, setRead] = useState(false);
  const [saveFailure, setSaveFailure] = useState<string>();
  // Why the document on disk could not be read (issue #1072). Distinct from
  // `saveFailure` in the state even though the two share one output: this one is
  // a standing condition of the FILE rather than the outcome of one write, so it
  // must survive the `setSaveFailure(undefined)` every save begins with —
  // otherwise the explanation blinks out on the click and returns when the
  // refusal lands, which is the one moment the user is reading it.
  const [documentProblem, setDocumentProblem] = useState<string>();
  // Whether the user has already changed something. The initial load is async,
  // so without this a choice made before it resolves is silently overwritten by
  // the document that was on disk when the app started — the change appears to
  // take, then reverts a moment later.
  const edited = useRef(false);
  // Saves run one at a time, and only the newest one's outcome is applied.
  //
  // Without this, two rapid choices both went out at once: they could reach the
  // disk in either order, and whichever *response* arrived last replaced React
  // state — so a stale document could win twice over. Today the only
  // consequence is a stale appearance choice; it matters more once the document
  // holds a daemon endpoint (#741) or a backend selection (#802). The
  // cross-process half of the same problem — two app windows racing on one
  // field — is #828, and is not something a hook can fix.
  const queue = useRef<Promise<void>>(Promise.resolve());
  const newest = useRef(0);

  useEffect(() => {
    let cancelled = false;
    void getSettings()
      .then((snapshot) => {
        if (cancelled) return;
        // The path is always worth taking; the document is not, if the user has
        // already moved on from it.
        setPath(snapshot.path);
        // So is the reason the document could not be read: it describes the file
        // rather than the in-memory document, so an edit made while the read was
        // in flight does not make it stale — that edit is exactly what the Rust
        // side is about to refuse to save.
        setDocumentProblem(snapshot.problem);
        if (!edited.current) setSettings(snapshot.settings);
        setRead(true);
      })
      // Swallowed, and `settings` keeps the defaults it was seeded with — a
      // desktop whose settings could not be read is still usable.
      //
      // This is the IPC failing, which is a different thing from the DOCUMENT
      // being unreadable: that one resolves normally and arrives as
      // `snapshot.problem` above (issue #1072). There is still no surface for
      // this one, and inventing a sentence for "the bridge did not answer" would
      // be inventing one for a condition the user cannot act on.
      //
      // This is now the ORDINARY failure path rather than an exotic one, and
      // issue #845 is what moved it: `TauriDeckBridge.getSettings` used to
      // answer an IPC failure with the same defaults itself, so a rejection
      // here could only be a bridge with no settings at all. It propagates now,
      // because a fabricated document saying mode "system" is indistinguishable
      // from a real one — which is exactly why `read` below is NOT set here.
      .catch(() => undefined)
      .finally(() => { if (!cancelled) setLoaded(true); });
    return () => { cancelled = true; };
  }, [getSettings]);

  const save = useCallback((next: DesktopSettingsDto) => {
    // Applied first, written behind it. PRD #743 requires the appearance change
    // to be visible with no restart, and waiting for a disk write to repaint
    // would put a round trip between the click and the theme.
    edited.current = true;
    setSettings(next);
    setSaveFailure(undefined);

    const ticket = newest.current + 1;
    newest.current = ticket;
    // Chained rather than fired: the next write starts only once this one has
    // settled, so the last choice made is the last one on disk. The inner
    // handlers never reject, so one failed save cannot break the chain for
    // every save after it.
    queue.current = queue.current.then(() => saveSettings(next)
      .then((written) => {
        // Any save that came back at all means the document on disk is one this
        // build can read: `save_to` refuses before writing otherwise. True of a
        // superseded response too, so this is cleared before the ticket check —
        // the document's state is not a property of which write won.
        setDocumentProblem(undefined);
        // A superseded response is dropped rather than applied — it is an
        // older document, and the user has already moved past it.
        if (newest.current === ticket) setSettings(written);
      })
      .catch((cause: unknown) => {
        // Deliberately NOT reverted. The user asked for this and can see it;
        // what failed is making it survive a restart, and saying so is more
        // use than silently undoing a choice they just made.
        //
        // Superseded failures are dropped for the same reason as superseded
        // successes: the message would be about a choice no longer on screen.
        if (newest.current !== ticket) return;
        const message = cause instanceof Error ? cause.message : String(cause);
        // The lead-in is composed here rather than in each panel, because a
        // panel now renders `saveError` verbatim: the other thing that reaches
        // that prop is a document problem, which is already a whole sentence and
        // must not acquire a "saving it failed" preamble it has not earned.
        setSaveFailure(`This change is applied, but saving it failed, so it will not survive a restart. ${message}`);
      }));
  }, [saveSettings]);

  // `edited` is a ref, and this reads it during render — safe here, and only
  // here, because it is monotonic (false to true, never back) and every write
  // to it is paired with a `setSettings` in the same call, so the render that
  // observes the new value is one React was already going to perform.
  return { settings, path, loaded, chosen: read || edited.current, saveError: saveFailure ?? documentProblem, problem: documentProblem, save };
}
