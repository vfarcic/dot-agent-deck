/**
 * Make everything that is not the agent pane genuinely inert while it is open,
 * and put focus inside it (PRD #1105's security audit).
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
import { useEffect, useRef } from "react";

export function useInertBackground<T extends HTMLElement>(open: boolean) {
  const ref = useRef<T>(null);
  useEffect(() => {
    const node = ref.current;
    if (!open || !node) return;

    const marked: Element[] = [];
    let child: Element = node;
    let parent = child.parentElement;
    while (parent) {
      for (const sibling of Array.from(parent.children)) {
        if (sibling === child) continue;
        if (sibling.hasAttribute("inert")) continue;
        sibling.setAttribute("inert", "");
        marked.push(sibling);
      }
      if (parent === document.body) break;
      child = parent;
      parent = parent.parentElement;
    }

    // Focus containment's other half. `inert` removes the background from the
    // tab order, but a control that ALREADY had focus when the pane opened —
    // the overview row's own maximise button is the ordinary case, since it is
    // a background element in the sibling-pane layout — is merely blurred by
    // it, leaving focus on `<body>` and the first Tab landing wherever the
    // document happens to start. Moving it into the pane is what makes the
    // dialog behave like one.
    if (!node.contains(document.activeElement)) node.focus();

    return () => {
      for (const element of marked) element.removeAttribute("inert");
    };
  });
  return ref;
}
