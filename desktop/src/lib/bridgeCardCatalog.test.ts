import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { beforeEach, describe, expect, it, vi } from "vitest";

/*
 * Issue #2399 review, finding 1 — the production Tauri bridge of the Card
 * actions editor. The editor's own tests replace the runtime with fixtures, so
 * these pin what the real bridge sends: the command name, its arguments (the
 * deck the editor shows, and the edit or `null` for a read), a refusal as an
 * Error carrying the daemon's reason unchanged, and the cards snapshot's
 * `catalogEdit` flag as the backend sent it.
 */

const invoke = vi.fn();

vi.mock("@tauri-apps/api/core", () => ({ invoke, Channel: class {} }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => undefined) }));

const DECK = "deck-000000000000cafe";
const CATALOG = { actions: [], errors: [], user_config: "/home/user/.config/dot-agent-deck/config.toml" };

describe("desktop/agent-cards/062: the Card actions editor's production bridge", () => {
  beforeEach(() => invoke.mockReset());

  /** Scenario: the editor reads deck DECK's card actions and then saves an added action. The bridge invokes `agent_card_catalog_edit` with that deck and no edit for the read, then with the same deck and the edit exactly as given, and hands back the catalog the backend answered. */
  it("reads and edits through agent_card_catalog_edit with the deck and the edit as given", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    invoke.mockResolvedValue(CATALOG);
    const bridge = new TauriDeckBridge();
    expect(await bridge.readCardCatalog(DECK, "process-a")).toEqual(CATALOG);
    expect(invoke).toHaveBeenLastCalledWith("agent_card_catalog_edit", { deckId: DECK, instanceId: "process-a", edit: null });
    const edit = { kind: "add" as const, action: { name: "ship", description: "Offer when it is ready.", prompt: "Ship it.\nThen update your card.", icon: "rocket", confirm: true, always: false } };
    expect(await bridge.editCardCatalog(DECK, edit, "process-a")).toEqual(CATALOG);
    expect(invoke).toHaveBeenLastCalledWith("agent_card_catalog_edit", { deckId: DECK, instanceId: "process-a", edit });
    for (const removal of [{ kind: "remove" as const, name: "ship" }, { kind: "hide" as const, name: "merge" }, { kind: "restore" as const, name: "merge" }]) {
      await bridge.editCardCatalog(DECK, removal, "process-a");
      expect(invoke).toHaveBeenLastCalledWith("agent_card_catalog_edit", { deckId: DECK, instanceId: "process-a", edit: removal });
    }
    expect(invoke).toHaveBeenCalledTimes(5);
  });

  /** Scenario: the editor reads and saves card actions listed by daemon process `process-a` (PR #2491 review, Qodo 4). The bridge passes that process as `instanceId` beside the deck, for the read and for the edit, so the backend binds both to it. */
  it("binds a read and an edit to the daemon process it names", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    invoke.mockResolvedValue(CATALOG);
    const bridge = new TauriDeckBridge();
    await bridge.readCardCatalog(DECK, "process-a");
    expect(invoke).toHaveBeenLastCalledWith("agent_card_catalog_edit", { deckId: DECK, instanceId: "process-a", edit: null });
    const removal = { kind: "remove" as const, name: "ship" };
    await bridge.editCardCatalog(DECK, removal, "process-a");
    expect(invoke).toHaveBeenLastCalledWith("agent_card_catalog_edit", { deckId: DECK, instanceId: "process-a", edit: removal });
  });

  /** Scenario: the editor asks to read, add and remove card actions naming no daemon process — `undefined`, or the empty id a cards snapshot without one carries (PR #2491 audit, A1). Each is refused before anything is invoked, in the backend's own words, the edits saying nothing was saved; nothing goes unbound to whichever process answers the deck. */
  it("desktop/agent-cards/132: refuses a read or an edit naming no daemon process, invoking nothing", async () => {
    const { TauriDeckBridge, CARD_CATALOG_DAEMON_UNNAMED } = await import("./bridge");
    const backend = readFileSync(resolve("src-tauri/src/lib.rs"), "utf8");
    expect(backend).toContain(`"${CARD_CATALOG_DAEMON_UNNAMED}";`);
    invoke.mockResolvedValue(CATALOG);
    const bridge = new TauriDeckBridge();
    const caught = (promise: Promise<unknown>) => promise.then(() => undefined, (error: unknown) => error as Error);
    for (const instanceId of [undefined, ""]) {
      expect((await caught(bridge.readCardCatalog(DECK, instanceId)))?.message).toBe(CARD_CATALOG_DAEMON_UNNAMED);
      const add = { kind: "add" as const, action: { name: "ship", description: "d", prompt: "p", confirm: false, always: false } };
      expect((await caught(bridge.editCardCatalog(DECK, add, instanceId)))?.message).toBe(`${CARD_CATALOG_DAEMON_UNNAMED}; nothing was saved`);
      expect((await caught(bridge.editCardCatalog(DECK, { kind: "remove", name: "ship" }, instanceId)))?.message).toBe(`${CARD_CATALOG_DAEMON_UNNAMED}; nothing was saved`);
    }
    expect(invoke).not.toHaveBeenCalledWith("agent_card_catalog_edit", expect.anything());
  });

  /** Scenario: the backend refuses the edit with the daemon's reason, as a string, as Tauri rejects. The bridge rejects with an Error whose message is exactly that reason. */
  it("rejects with the daemon's reason unchanged", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const reason = "catalog.actions \"Ship It\": bad name; use 1 to 40 lowercase letters, digits and '-', starting with a letter or digit";
    // Only the editor's command refuses: the bridge makes other calls of its own.
    invoke.mockImplementation(async (command: string) => {
      if (command === "agent_card_catalog_edit") throw reason;
      return undefined;
    });
    const bridge = new TauriDeckBridge();
    const refused = await bridge.editCardCatalog(DECK, { kind: "remove", name: "Ship It" }, "process-a").catch((error: unknown) => error);
    expect(refused).toBeInstanceOf(Error);
    expect((refused as Error).message).toBe(reason);
    expect(invoke).toHaveBeenCalledWith("agent_card_catalog_edit", { deckId: DECK, instanceId: "process-a", edit: { kind: "remove", name: "Ship It" } });
  });

  /** Scenario: the backend pushes two decks' cards, one whose daemon advertised the editor and one whose daemon did not. The snapshot the dashboard reads says `catalogEdit` true for the first and false for the second, and false when the field is missing. */
  it("carries the daemon's catalogEdit flag into the cards snapshot", async () => {
    const { normalizeAgentCards } = await import("./bridge");
    const base = { deckId: DECK, instanceId: "process-a", revision: 1, entries: [], catalogErrors: [], tree: true, batch: true };
    expect(normalizeAgentCards({ ...base, catalogEdit: true })?.catalogEdit).toBe(true);
    expect(normalizeAgentCards({ ...base, catalogEdit: false })?.catalogEdit).toBe(false);
    expect(normalizeAgentCards(base)?.catalogEdit).toBe(false);
    expect(normalizeAgentCards({ ...base, catalogEdit: "yes" })?.catalogEdit).toBe(false);
  });

  /** Scenario: the backend fails an edit saying its answer was lost and it may have been saved, then refuses another with the daemon's reason, then fails a read the same uncertain way. Only the uncertain edit is marked `outcome: "unknown"`; the refusal and the read are not, and the words the bridge looks for are the ones the backend writes (issue #2399 review 8). */
  it("desktop/agent-cards/071: marks an edit whose answer was lost as of unknown outcome, and a refusal as not", async () => {
    const { TauriDeckBridge, CARD_CATALOG_EDIT_MAY_HAVE_SAVED } = await import("./bridge");
    const backend = readFileSync(resolve("src-tauri/src/lib.rs"), "utf8");
    expect(backend).toContain(`const CARD_CATALOG_EDIT_MAY_HAVE_SAVED: &str = "${CARD_CATALOG_EDIT_MAY_HAVE_SAVED}";`);
    const lost = `the deck did not answer within 15s; ${CARD_CATALOG_EDIT_MAY_HAVE_SAVED}, so check the list before saving again`;
    const failures = [lost, "an action named \"ship\" already exists; edit it instead", lost];
    invoke.mockImplementation(async (command: string) => {
      if (command === "agent_card_catalog_edit") throw failures.shift();
      return undefined;
    });
    const bridge = new TauriDeckBridge();
    const caught = (promise: Promise<unknown>) => promise.then(() => undefined, (error: unknown) => error as Error & { outcome?: string });
    const unsure = await caught(bridge.editCardCatalog(DECK, { kind: "hide", name: "merge" }, "process-a"));
    expect(unsure?.message).toBe(lost);
    expect(unsure?.outcome).toBe("unknown");
    const refused = await caught(bridge.editCardCatalog(DECK, { kind: "add", action: { name: "ship", description: "d", prompt: "p", confirm: false, always: false } }, "process-a"));
    expect(refused?.outcome).toBeUndefined();
    const read = await caught(bridge.readCardCatalog(DECK, "process-a"));
    expect(read?.outcome, "a read writes nothing, so it is never of unknown outcome").toBeUndefined();
  });
});
