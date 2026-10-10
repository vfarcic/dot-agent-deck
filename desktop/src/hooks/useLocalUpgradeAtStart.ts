import { useCallback, useEffect, useRef, useState } from "react";

import type { UpgradeTarget } from "../components/UpgradeDialog";
import { deckName } from "../lib/displayText";
import { localUpgradeAtStart } from "../lib/upgrade";
import type { DeckRuntimeState } from "../types";

/**
 * Issue #1636 — the app upgrading the local daemon on its own, as the TUI does
 * when it starts against an older one: when the local deck is connected and
 * the crate says its daemon runs an older release than this app, the Upgrade
 * dialog opens already running. With nothing running on the daemon it simply
 * restarts onto this app's build; with agents or orchestration roles running,
 * the daemon names them and the dialog asks **Restart now** / **Keep current
 * daemon**, exactly as a pressed Upgrade does.
 *
 * At most once per daemon version while the app runs (`localUpgradeAtStart`'s
 * key): a Keep, a failure or a closed dialog is not asked again in a loop, and
 * Upgrade on the local deck stays there to do it on demand. A later launch
 * checks again, as the TUI does at each launch.
 *
 * Live mode only — the fixture preview plays decks, it does not run daemons.
 */
export function useLocalUpgradeAtStart(runtime: Pick<DeckRuntimeState, "mode" | "fleet" | "upgradeDaemon">): { target?: UpgradeTarget; close: () => void } {
  const [target, setTarget] = useState<UpgradeTarget>();
  const handled = useRef(new Set<string>());
  const found = runtime.mode === "live" && runtime.upgradeDaemon ? localUpgradeAtStart(runtime.fleet) : undefined;
  const key = found?.key;
  const open = target !== undefined;
  useEffect(() => {
    if (!found || open || handled.current.has(found.key)) return;
    handled.current.add(found.key);
    const { connection } = found;
    setTarget({ deckId: connection.deckId, deckName: deckName(connection), kind: "local-upgrade", offer: connection.upgradeOffer });
    // `key` stands for `found`: a new snapshot of the same daemon is not a new one.
  }, [key, open]);
  const close = useCallback(() => setTarget(undefined), []);
  return { target, close };
}
