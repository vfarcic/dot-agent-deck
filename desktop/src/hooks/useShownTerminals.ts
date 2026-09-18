/**
 * The single declaration of what is on screen (PRD #1105 M4).
 *
 * # The contract this exists to make structural
 *
 * `setShownTerminals` is declarative and must be called **once per render
 * commit with the whole shown set** — never once per tile. Its own doc comment
 * carries the arithmetic (`desktop/src/lib/bridge.ts`): nine single-id calls
 * would leave eight of the nine warm and evict five of them, which is the same
 * broken deck `MAX_WARM_TERMINALS` exists to avoid.
 *
 * Before M4 that rule was kept by two independent effects agreeing not to be
 * mounted at the same time — the deck's, derived from its tabs, and the
 * overview's, which declared the empty set on mount. The agent pane broke the
 * agreement: it sits *over* a base screen rather than instead of one, so the
 * overview's `[]` and the pane's `[agentId]` are two declarations in one
 * commit. Which of them the bridge saw last then decided whether the pane had a
 * terminal, and neither outcome fails loudly.
 *
 * So the declaration moved to whichever component can see the base screen and
 * the pane **together**, and this hook is the mechanism it moved into. There is
 * one owner per mounted screen tree and the two trees are mutually exclusive:
 * `DeckSurface` owns the deck's set, `DeckShell` owns the overview's.
 *
 * # `undefined` means "not the owner", and it is not the empty set
 *
 * A component that is mounted but not the owner in its current state passes
 * `undefined`, which makes **no call at all** — including on the commit that
 * hands ownership over, where firing would be a second declaration racing the
 * new owner's first. `[]` is a real declaration and a very expensive one: it
 * flushes the warm set to zero, detaching everything (`bridge.ts`'s
 * `overflow = this.warm.size` branch). The two must never collapse together,
 * which is why the dependency below is `agentIds?.join(…)` — `undefined` for
 * "no declaration" and `""` for "declare nothing shown" — and not a joined
 * string with an empty-array default.
 *
 * # Why the ids travel in a ref
 *
 * The effect has to fire when the SET changes and not on every render, so it
 * keys on the joined string, which is stable across renders that change
 * nothing. The array itself is fresh every render, so it cannot be the
 * dependency and is read from a ref instead.
 *
 * The key is a dependency key and nothing else — never split back apart. Agent
 * ids are raw daemon identities, not display strings, so an id containing a
 * newline would come back out of a `split` as two shown agents. Each element is
 * itself an {@link agentKey}, because the declaration names `(deckId, agentId)`
 * pairs since PRD #1105's cross-deck pane: a set that changed only which DECK
 * an id is on would otherwise produce an identical key and make no call.
 */
import { useEffect, useRef } from "react";
import { agentKey } from "../lib/agentKey";
import type { AgentTarget } from "../types";

export function useShownTerminals(
  setShownTerminals: (targets: AgentTarget[]) => Promise<void>,
  targets: AgentTarget[] | undefined,
): void {
  const key = targets?.map((target) => agentKey(target.deckId, target.agentId)).join("\n");
  const latest = useRef(targets);
  latest.current = targets;
  useEffect(() => {
    const declared = latest.current;
    // Not the owner in this state. See the header: this is the one case that
    // must not reach the bridge, because `[]` is itself a declaration.
    if (!declared) return;
    void setShownTerminals(declared);
  }, [setShownTerminals, key]);
}
