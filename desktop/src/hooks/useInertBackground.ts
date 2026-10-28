/**
 * Make everything that is not the open dialog genuinely inert while it is open,
 * put focus inside it, and give focus back to the opener on close (PRD #1105's
 * security audit).
 *
 * # Two callers, one mechanism
 *
 * Written for the agent pane, which is what the notes below argue from, and
 * since PRD #1223 also used by `NewAgentDialog` — the other surface here that
 * declares `role="dialog" aria-modal="true"` over a base screen that stays
 * mounted, and had the same false claim for the same reason. Reusing it rather
 * than writing a second trap is deliberate: a per-dialog Tab-cycling handler
 * would contain the keyboard and leave the background CLICKABLE, and the two
 * implementations would then disagree about what "modal" means on a screen that
 * can show both.
 *
 * **Not every dialog here is fenced, and saying so is the honest version of
 * "a repo-wide convention".** `ConfirmDialog`, `UpgradeDialog`,
 * `SelfUpgradeDialog` and `OutputReader` are on the stack (issue #1709), with
 * their own way out as `close`, so voice's "close" dismisses them and no longer
 * reaches past one to the overlay beneath it.
 * `SettingsSheet` and `ConfigurationPanels`'s four sheets are on the stack
 * through {@link useOverlaySheet} (PR #1680 review), and fence only while
 * raised over the agent pane, the pull request browser or the output reader;
 * otherwise they fence nothing, as before. PRD #1401's
 * pull request browser is the third caller, and the first that opens over
 * another: it comes up over a still-mounted pane, so only the topmost open
 * caller fences (see `stack` below) and the pane beneath it is background like
 * the rest. `App`'s command palette and shortcut guide joined for that reason
 * (PR #1680 review): the deck's window keys open them over the pull request
 * browser, inside the deck it had fenced, so without a fence of their own they
 * came up inert. The rest are left alone deliberately — the surfaces wired up
 * are the ones whose PRDs came with the audit that found this, or that open
 * over one of them — rather than because they are exempt. `VoiceControlPanel` is the one
 * that is exempt: it omits `aria-modal` on purpose (see its own note), so it
 * claims nothing it does not do.
 *
 * # Why `aria-modal` alone was a false claim
 *
 * `AgentPaneFrame` renders `role="dialog"` with `aria-modal="true"`, which
 * tells assistive technology that the rest of the interface is unavailable. It
 * was not. The base screen stays **mounted** underneath — that is the whole
 * no-flicker design, not an oversight — so every control on it kept its place
 * in the tab order behind a full-window overlay: the rail, the tiles, and the
 * `DeckSelector` in the topbar.
 *
 * That last one is the security half rather than an a11y nit. Reaching the deck
 * selector under an open pane and choosing another deck retargets the app while
 * the pane keeps claiming the agent the user opened — same role, same display
 * text, no deck identity anywhere in the dialog. The pane is separately fenced
 * on identity in `DeckShell`, and these are two protections rather than one:
 * the fence stops a retargeted pane existing, and this stops the retargeting
 * being reachable from behind a dialog that says it is modal.
 *
 * # Marking siblings, rather than one subtree
 *
 * `inert` is inherited down the flat tree and a descendant cannot escape it, so
 * "mark the base screen inert" is not available on the deck path: there the
 * pane IS a tile inside the base screen, promoted in place so that its xterm
 * survives. What is available is the standard modal-dialog treatment — walk
 * from the pane up to `document.body` and mark every element that is NOT on
 * that path. The pane's own ancestors stay live, everything beside them does
 * not, and the result is the same interface a real `<dialog>` modal produces.
 *
 * Doing it by walking rather than by an `inert` prop on each background region
 * is what stops it rotting: a region added to the deck later is inerted with no
 * edit here, where a list of a dozen JSX sites would silently omit it.
 *
 * **The effect deliberately has no dependency array.** Background content
 * appears while a pane is open — a toast, a connection banner, a newly reported
 * agent's tile — and each arrives on a render commit. Re-marking on every
 * commit while open is what covers them; the walk is a few dozen nodes and
 * everything it touches is idempotent.
 *
 * Only elements this hook marked are unmarked on cleanup, so an element that
 * was already inert for some other reason keeps its own state.
 *
 * # The one exemption: the voice surface is a PEER, not background
 *
 * PRD #802's voice trigger and the report beside it are siblings of the screen
 * switch in `DeckShell`, so the walk above reaches them and marks them like any
 * other background region — which is what it should do for a region, and wrong
 * for a second surface the user is meant to reach. The cost was not cosmetic:
 * `inert` removes an element from hit testing as well as from the tab order, so
 * on the `agent` screen the trigger was unclickable behind the pane, and
 * `close` — the command that dismisses whatever is on top — was unreachable by the
 * surface that dispatches it. The report carries the marker for the same
 * reason and not by habit: the Undo inside it is a control, and an Undo that
 * cannot be clicked is a picture of one.
 *
 * **The exempt elements are no longer a dialog, and the exemption did not
 * widen when they stopped being one.** M6's voice surface was a trigger plus a
 * modal-looking dialog; then a trigger plus a transient report, because
 * continuous voice control cannot be gated behind something that has to be
 * dismissed between utterances; and now **one** element — the reserved
 * `.voice-row` holding both, which is what stopped the report overlapping the
 * pane it was reporting on. What is matched is the marker, not a role, so each
 * shape change cost nothing here.
 *
 * Read the count going from two to one as the exemption NARROWING rather than
 * moving: it is one marked sibling containing the same two controls, so what the
 * walk skips is the same subtree it skipped before. The trigger and the Undo
 * inside the report are still the complete set of controls reachable outside an
 * open pane, which `AgentPaneDeckIdentity.test.tsx` asserts by equality rather
 * than by a filter.
 *
 * So elements carrying {@link VOICE_PEER_PROPS} are skipped, and nothing else
 * is. It is an allow-list of one marker rather than a rule about roles or
 * dialogs: a second peer added later has to be exempted here deliberately,
 * which is the review this hook's audit asked for.
 *
 * **What the hook still guarantees is unchanged for everything else.** Every
 * element that is not an ancestor of the pane and does not carry that marker is
 * still inerted on every commit, the `DeckSelector` above all — so the
 * retargeting attack the audit found stays unreachable, and the exemption
 * cannot widen by accident: it is matched on the sibling itself, so a marked
 * element nested inside a background region is still inert with its region.
 *
 * The two surfaces do not fight over `Escape` either. The voice surface binds
 * no WINDOW key: its one keyboard surface is the numbered choice's dialog
 * (PRD #1261), which handles `Escape` and the digits on its own element and
 * stops them there, so `DeckShell`'s window listener for the pane sees an
 * `Escape` only when focus is outside that dialog.
 *
 * **`setAttribute` rather than the `inert` IDL property, and that is not
 * style.** Measured against this repo's jsdom (30.x): `"inert" in
 * HTMLElement.prototype` is `false`, so `element.inert = true` sets a plain
 * expando, writes no attribute, and leaves the vitest guard asserting nothing.
 * The same probe shows jsdom implements none of the FOCUS semantics either —
 * `focus()` on a button under an inert ancestor still makes it
 * `document.activeElement` — which is why what the attribute *does* is pinned
 * in the browser tier (`desktop/e2e/agent-pane-modal.spec.ts`) and only its
 * presence is pinned here.
 */
import { useEffect, useRef, useSyncExternalStore } from "react";

/**
 * The marker that makes an element a peer of the agent pane rather than
 * background, spread onto the voice trigger and its report in
 * {@link ../components/VoiceControlPanel!VoiceControlPanel}.
 *
 * Exported as props to spread rather than written as a literal attribute at
 * each site, so the two elements and the selector below cannot drift apart
 * under a rename — a drift that would silently restore the bug, since an
 * unmarked trigger is simply inert again.
 */
export const VOICE_PEER_PROPS = { "data-modal-peer": "voice" } as const;

/** The same marker, as the selector the walk tests each sibling against. */
const VOICE_PEER_SELECTOR = `[data-modal-peer="${VOICE_PEER_PROPS["data-modal-peer"]}"]`;

/**
 * Whether `element` is inside the voice surface ({@link VOICE_PEER_PROPS}).
 * For a dialog the voice surface can host as well as a screen — `ConfirmDialog`
 * (issue #1709) — which stays off the stack there: the voice row is raised over
 * the stack's layers while it holds a dialog, and that dialog answers its own
 * "confirm" and "cancel", so ordering and fencing it here would contradict both.
 */
export function insideVoicePeer(element: Element): boolean {
  return element.closest(VOICE_PEER_SELECTOR) !== null;
}

/**
 * PR #1680 review (Qodo) — the layer each overlay on the stack paints in: the
 * `z-index` of its backdrop in `styles.css`, which `overlayLayers.test.ts`
 * reads back so the two cannot drift. The stack is ordered as the browser
 * paints: by layer, and within a layer by document order, so "on top" here is
 * what is drawn on top.
 *
 * Open order is not that, at either level. A sheet is drawn beneath the
 * agent's pane and the pull request browser whenever it opened, so a sheet
 * opened BEFORE them is under them on screen however recent it is. And two
 * overlays on ONE layer — the command menu and the shortcut guide are both
 * `.dialog-backdrop` — are painted in the order they sit in the document,
 * whichever opened last: ordering those by open time left the guide drawn in
 * front of the menu but fenced behind it (PR #1680 review, finding 14).
 * `desktop/e2e/overlay-stack.spec.ts` asks the real stylesheet which is in
 * front, since jsdom paints nothing.
 */
export const OVERLAY_LAYER = {
  /** `.sheet-backdrop`: Settings and the configuration sheets, when not raised. */
  sheet: 80,
  /** `.reader-overlay`: an agent's output reader, above a sheet so that a sheet opened over it is raised, as over the pane. */
  reader: 85,
  /** `.agent-pane-overlay`. */
  pane: 90,
  /** `.pr-browser-overlay`. */
  pullRequest: 92,
  /** `.sheet-backdrop.is-raised`: a sheet opened over the pane or the pull request browser, which the user just asked for. */
  raisedSheet: 93,
  /** `.dialog-backdrop`: the New agent dialog, the command menu, the shortcut sheet, and the confirmation and upgrade dialogs. */
  dialog: 100,
} as const;

/**
 * One open overlay on the stack: whatever this hook was given, once it has
 * rendered, the layer it paints in, and when it opened (`0` while it is not on
 * the stack). The node is refreshed on every commit, so an overlay whose root
 * element was replaced is found by its new one. `close` is how voice's "close"
 * dismisses it when it has no member of its own; `fences` is false for a sheet
 * that is not raised, which fences nothing (see {@link useOverlaySheet}).
 * A `close` that cannot close right now returns the sentence saying why, which
 * voice reports instead of "Closed." (issue #1709).
 */
type OverlayClose = () => string | void;
type Fenced = { node: HTMLElement | null; layer: number; opened: number; fences: boolean; close?: OverlayClose };

/**
 * PRD #1401 — every open overlay. The topmost FENCING one is the only one that
 * fences; the ones beneath it are background to it like anything else.
 *
 * **Why one owner rather than one walk per overlay.** The in-app pull request
 * browser opens over a still-mounted agent pane, and both call this hook. With
 * a walk each, each marked the other: the pane's walk reached the browser as
 * one of its siblings and the browser's reached the pane, so the frontmost
 * toolbar was as inert as the pane under it and neither could be used. A
 * per-overlay exemption ("skip other dialogs") would have been the broad hole
 * the audit closed — so the walk is done once, for the topmost overlay, and
 * the marks are held here rather than per instance, so no overlay's cleanup
 * can unmark what the topmost one still needs marked.
 *
 * Ordered as the browser paints them ({@link OVERLAY_LAYER}, then document
 * order). Closing the topmost hands the fence back to the one beneath it on
 * the same commit.
 */
const stack: Fenced[] = [];
/** How many overlays have opened, so a sheet can tell what was already up when it opened. */
let opens = 0;
/** Bumped whenever an overlay comes onto the stack or leaves it, for the sheets that place themselves by it. */
let stackVersion = 0;
const stackListeners = new Set<() => void>();
function stackChanged() {
  stackVersion += 1;
  for (const listener of stackListeners) listener();
}
function subscribeToStack(listener: () => void) {
  stackListeners.add(listener);
  return () => { stackListeners.delete(listener); };
}
const readStackVersion = () => stackVersion;
/** The elements the fence marked, and only those, so an element inert for some other reason keeps its own state. */
let marked = new Set<Element>();

/**
 * Which of two overlays the browser paints first: the lower layer, and within
 * one layer the one earlier in the document — positioned boxes with equal
 * `z-index` in one stacking context paint in tree order, later on top.
 */
function paintOrder(a: Fenced, b: Fenced): number {
  if (a.layer !== b.layer) return a.layer - b.layer;
  if (!a.node || !b.node || a.node === b.node) return 0;
  return a.node.compareDocumentPosition(b.node) & Node.DOCUMENT_POSITION_FOLLOWING ? -1 : 1;
}

/** The open overlays that have a node in the document, bottom first. */
function ordered(): Fenced[] {
  return stack.filter((entry) => entry.node?.isConnected).sort(paintOrder);
}

/** The topmost open overlay that fences. */
function topmost(): Fenced | undefined {
  return ordered().filter((entry) => entry.fences).at(-1);
}

/** Whether `element` is one of the open overlays on the stack, or inside one. */
export function stackedOverlay(element: Element): boolean {
  return ordered().some((entry) => entry.node?.contains(element));
}

/**
 * Whether an open overlay on the stack is drawn over the one `element` is (or
 * is in). `false` for an element in no overlay on the stack.
 */
export function overlayAbove(element: Element | null): boolean {
  if (!element) return false;
  const open = ordered();
  let index = -1;
  open.forEach((entry, at) => { if (entry.node?.contains(element)) index = at; });
  return index >= 0 && index < open.length - 1;
}

/** How to close the topmost open overlay, when it registered one (see `Fenced`). */
export function topmostOverlayClose(): OverlayClose | undefined {
  return ordered().at(-1)?.close;
}

/**
 * Mark everything that is not the topmost overlay or on its path to `<body>`,
 * and unmark what the previous pass marked and this one does not. Idempotent:
 * every open overlay calls it on every one of its commits.
 */
function fence() {
  const next = new Set<Element>();
  const node = topmost()?.node;
  if (node) {
    let child: Element = node;
    let parent = child.parentElement;
    while (parent) {
      for (const sibling of Array.from(parent.children)) {
        if (sibling === child) continue;
        // The one exemption, and the only one. See the note above.
        if (sibling.matches(VOICE_PEER_SELECTOR)) continue;
        // Inert for a reason of its own, which is not ours to undo later.
        if (sibling.hasAttribute("inert") && !marked.has(sibling)) continue;
        next.add(sibling);
      }
      if (parent === document.body) break;
      child = parent;
      parent = parent.parentElement;
    }
  }
  for (const element of marked) if (!next.has(element)) element.removeAttribute("inert");
  for (const element of next) if (!element.hasAttribute("inert")) element.setAttribute("inert", "");
  marked = next;
}

/**
 * Fence the background while `open`, as the overlay of `layer`
 * ({@link OVERLAY_LAYER}). `close`, when given, is how voice's "close"
 * dismisses this overlay while it is the topmost one: for the overlays that
 * have no voice member of their own.
 */
export function useInertBackground<T extends HTMLElement>(open: boolean, layer: number, close?: OverlayClose) {
  return useOverlay<T>(open, layer, true, close).ref;
}

/**
 * PR #1680 review (Qodo) — Settings and the configuration sheets on the stack.
 *
 * A sheet is drawn at {@link OVERLAY_LAYER}'s `sheet`, beneath the agent's pane,
 * the pull request browser and the output reader, and fences nothing — what it
 * did before it was on the stack — unless it opened over one of THOSE: the overlays a sheet is
 * otherwise painted under. Then it is what the user just asked for, so it is
 * `raised`: drawn above them (`.sheet-backdrop.is-raised`) and fencing them,
 * since it would otherwise be inert under their fence.
 *
 * **Decided from the stack, not from a render-time look at the topmost
 * overlay, and kept current** (PR #1680 review, finding 15). The command menu
 * runs its entry and closes itself in one batch, so the sheet it opens renders
 * while the menu is still on the stack: "is anything fenced up?" answered then
 * was "yes, the menu", and the answer was latched — Settings chosen from the
 * menu on a plain deck came up raised, fencing the deck, and a pane opened
 * after it drew underneath it. The menu, the shortcut guide and the other
 * dialogs are on a layer ABOVE a raised sheet, so they never call for raising
 * one, and are not asked. And the answer follows the stack: a pane or a page
 * opened after the sheet does not raise it (it is drawn over the sheet, as it
 * should be), and the one it was raised over closing lowers it again.
 */
export function useOverlaySheet<T extends HTMLElement>(open: boolean, close: () => void): { ref: React.RefObject<T | null>; raised: boolean } {
  useSyncExternalStore(subscribeToStack, readStackVersion);
  const self = useRef<Fenced>(null);
  const raised = open && opensOverPaneOrPage(self.current);
  const { ref, entry } = useOverlay<T>(open, raised ? OVERLAY_LAYER.raisedSheet : OVERLAY_LAYER.sheet, raised, close);
  self.current = entry;
  return { ref, raised };
}

/**
 * Whether one of the overlays a sheet is painted under (the output reader,
 * the pane, the pull request browser) was already up when `sheet` opened. A
 * sheet not yet on the stack (`opened` 0, or no entry yet: the render that
 * opens it) counts everything up now.
 */
function opensOverPaneOrPage(sheet: Fenced | null): boolean {
  const opened = sheet?.opened ?? 0;
  return stack.some((entry) => entry !== sheet
    && entry.layer > OVERLAY_LAYER.sheet && entry.layer < OVERLAY_LAYER.raisedSheet
    && (opened === 0 || entry.opened < opened));
}

function useOverlay<T extends HTMLElement>(open: boolean, layer: number, fences: boolean, close?: OverlayClose) {
  const ref = useRef<T>(null);
  /** What had focus when this opened, so that closing can give it back. */
  const opener = useRef<Element | null>(null);
  const entry = useRef<Fenced>({ node: null, layer, opened: 0, fences, close });
  entry.current.layer = layer;
  entry.current.fences = fences;
  entry.current.close = close;

  /**
   * Captured FIRST — before the walk below, on the same commit, moves focus
   * inside. An effect declared after it would read a control of the dialog's
   * own and "restore" focus to an element that is about to be unmounted.
   */
  useEffect(() => {
    if (open) opener.current = document.activeElement;
  }, [open]);

  /**
   * Onto the stack when it opens, off it when it closes — and the fence
   * re-drawn for whatever is then on top. Declared before the walk, so that
   * an overlay is on the stack by the time its first walk asks who is topmost,
   * and before the focus restore, whose cleanup needs the overlay beneath
   * (where the opener usually is) already live again.
   */
  useEffect(() => {
    if (!open) return;
    const mine = entry.current;
    opens += 1;
    mine.opened = opens;
    stack.push(mine);
    stackChanged();
    return () => {
      const index = stack.indexOf(mine);
      if (index >= 0) stack.splice(index, 1);
      mine.node = null;
      mine.opened = 0;
      fence();
      stackChanged();
    };
  }, [open]);

  useEffect(() => {
    const node = ref.current;
    if (!open || !node) return;
    entry.current.node = node;
    fence();
    if (topmost() !== entry.current) return;

    // Focus containment's other half. `inert` removes the background from the
    // tab order, but a control that ALREADY had focus when the pane opened —
    // the overview row's own maximise button is the ordinary case, since it is
    // a background element in the sibling-pane layout — is merely blurred by
    // it, leaving focus on `<body>` and the first Tab landing wherever the
    // document happens to start. Moving it into the pane is what makes the
    // dialog behave like one. Only the topmost overlay does it: one beneath
    // must not take focus from the one the user is looking at.
    //
    // Focus on a voice peer is left where it is: the numbered choice's dialog
    // takes focus while it is open (PRD #1261), and this walk runs on every
    // commit, so pulling focus back here would take it from the dialog mid-answer.
    const active = document.activeElement;
    if (!node.contains(active) && !active?.closest(VOICE_PEER_SELECTOR)) node.focus();
  });

  /**
   * Focus containment's third half: give it back on close.
   *
   * The walk above takes focus off whatever opened this and puts it inside,
   * which is what makes the dialog behave like one — and left the user nowhere
   * on the way out. Closing with `Escape` from a dialog that had moved focus
   * inside leaves it on `<body>`, so the next Tab starts from the top of the
   * document rather than from the control just left: the maximise button of
   * the tile whose pane was open, or the overview's own New agent button.
   *
   * **Declared LAST, so that its cleanup runs after the un-marking one.**
   * React runs a commit's cleanups in the order their effects were declared,
   * and `focus()` on an element still carrying `inert` is a no-op — restoring
   * before the walk's cleanup has un-marked the background would silently
   * leave focus on `<body>`, which is exactly the state this exists to fix.
   *
   * `isConnected` rather than a bare call: a close that also unmounts the
   * opener is ordinary — a tile retired while its pane was open — and focusing
   * a detached element moves focus to `<body>` in some engines, undoing the
   * fallback a later dialog on the same commit may have just made. `<body>` is
   * excluded for the same reason it is in the walk: it is where focus already
   * is, not somewhere to put it.
   */
  useEffect(() => {
    if (!open || !fences) return;
    return () => {
      const element = opener.current;
      opener.current = null;
      if (element instanceof HTMLElement && element !== document.body && element.isConnected) element.focus();
    };
  }, [open, fences]);

  return { ref, entry: entry.current };
}
