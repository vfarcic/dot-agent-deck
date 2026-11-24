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
    expect(await bridge.readCardCatalog(DECK)).toEqual(CATALOG);
    expect(invoke).toHaveBeenLastCalledWith("agent_card_catalog_edit", { deckId: DECK, edit: null });
    const edit = { kind: "add" as const, action: { name: "ship", description: "Offer when it is ready.", prompt: "Ship it.\nThen update your card.", icon: "rocket", confirm: true, always: false } };
    expect(await bridge.editCardCatalog(DECK, edit)).toEqual(CATALOG);
    expect(invoke).toHaveBeenLastCalledWith("agent_card_catalog_edit", { deckId: DECK, edit });
    for (const removal of [{ kind: "remove" as const, name: "ship" }, { kind: "hide" as const, name: "merge" }, { kind: "restore" as const, name: "merge" }]) {
      await bridge.editCardCatalog(DECK, removal);
      expect(invoke).toHaveBeenLastCalledWith("agent_card_catalog_edit", { deckId: DECK, edit: removal });
    }
    expect(invoke).toHaveBeenCalledTimes(5);
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
    const refused = await bridge.editCardCatalog(DECK, { kind: "remove", name: "Ship It" }).catch((error: unknown) => error);
    expect(refused).toBeInstanceOf(Error);
    expect((refused as Error).message).toBe(reason);
    expect(invoke).toHaveBeenCalledWith("agent_card_catalog_edit", { deckId: DECK, edit: { kind: "remove", name: "Ship It" } });
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
});
