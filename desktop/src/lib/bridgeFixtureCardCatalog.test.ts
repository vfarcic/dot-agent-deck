import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, describe, expect, it } from "vitest";

/*
 * PR #2491 re-audit (F2) — the fixture bridge's Card actions editor models
 * the production contract: a read or an edit naming no daemon process is
 * refused in the Tauri bridge's own words, and one naming a process other
 * than the one whose cards the fixture deck shows is refused as the backend
 * refuses a daemon that replaced the one shown. Only the process shown reads
 * and edits the catalog.
 */

const search = window.location.search;
afterEach(() => window.history.replaceState({}, "", `/${search}`));

const caught = (promise: Promise<unknown>) => promise.then(() => undefined, (error: unknown) => error as Error);

describe("the fixture bridge's Card actions editor", () => {
  /** Scenario: on the editable tree scene, the editor's reads and edits name no process (`undefined`), the empty id, a process that is not the one whose cards the deck shows, and that one. The first two are refused in the Tauri bridge's words, the third as a replaced daemon, each edit adding that nothing was saved, and none of them changes the catalog; the process shown reads it and adds an action. */
  it("desktop/agent-cards/135: refuses a read or an edit naming no process or another process, as the production bridge does", async () => {
    window.history.replaceState({}, "", "/?fixture=1&state=docs&cards=tree");
    const { createDeckBridge, CARD_CATALOG_DAEMON_UNNAMED, CARD_CATALOG_DAEMON_REPLACED } = await import("./bridge");
    const backend = readFileSync(resolve("src-tauri/src/lib.rs"), "utf8");
    expect(backend.replace(/\s+/g, " ")).toContain(`CARD_CATALOG_DAEMON_REPLACED: &str = "${CARD_CATALOG_DAEMON_REPLACED}";`);
    const { FIXTURE_DAEMON_ID } = await import("../data/fixture");
    const bridge = createDeckBridge("fixture");
    const shown = (await bridge.agentCards?.(FIXTURE_DAEMON_ID))?.instanceId;
    expect(shown, "the scene's deck shows cards from a named process").toBeTruthy();
    const add = (name: string) => ({ kind: "add" as const, action: { name, description: "Offer when it applies.", prompt: "Do it.", confirm: false, always: false } });
    const before = await bridge.readCardCatalog!(FIXTURE_DAEMON_ID, shown);

    for (const [instanceId, words] of [[undefined, CARD_CATALOG_DAEMON_UNNAMED], ["", CARD_CATALOG_DAEMON_UNNAMED], ["wrong-process", CARD_CATALOG_DAEMON_REPLACED]] as const) {
      expect((await caught(bridge.readCardCatalog!(FIXTURE_DAEMON_ID, instanceId)))?.message, `read naming ${String(instanceId)}`).toBe(words);
      expect((await caught(bridge.editCardCatalog!(FIXTURE_DAEMON_ID, add(`unbound-${instanceId ?? "none"}`), instanceId)))?.message, `add naming ${String(instanceId)}`).toBe(`${words}; nothing was saved`);
      expect((await caught(bridge.editCardCatalog!(FIXTURE_DAEMON_ID, { kind: "hide", name: "go-ahead" }, instanceId)))?.message, `hide naming ${String(instanceId)}`).toBe(`${words}; nothing was saved`);
    }
    expect(await bridge.readCardCatalog!(FIXTURE_DAEMON_ID, shown), "no refused edit changed the catalog").toEqual(before);

    const added = await bridge.editCardCatalog!(FIXTURE_DAEMON_ID, add("bound"), shown);
    expect(added.actions.map((action) => action.name)).toContain("bound");
  });
});
