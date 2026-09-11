/**
 * The deck vocabulary on the webview's side — the field checks the Decks panel
 * types against (PRD #741 M7) and the selection value the Deck selector carries
 * (M9).
 */
import { describe, expect, it } from "vitest";
import { LOCAL_ENDPOINT_SELECTION, type EndpointSettingsDto } from "./bridge";
import {
  deckChoices,
  describeEndpoint,
  LOCAL_DECK_SELECTION,
  mintEndpointId,
  parseSelection,
  sameSelection,
  selectionToken,
} from "./endpoints";

const ID = "a1b2c3d4e5f60718";

function section(selection: string): EndpointSettingsDto {
  return {
    remote: [
      { host: "build-box.example.com", id: ID, port: 22, user: "vf" },
      { host: "relay.example.com", id: "0f1e2d3c4b5a6978", port: 2222 },
    ],
    selection,
  };
}

describe("the selection value", () => {
  it("round-trips both variants through their stored token", () => {
    expect(selectionToken(parseSelection(LOCAL_ENDPOINT_SELECTION))).toBe(LOCAL_ENDPOINT_SELECTION);
    expect(selectionToken(parseSelection(ID))).toBe(ID);
    expect(parseSelection(LOCAL_ENDPOINT_SELECTION)).toEqual(LOCAL_DECK_SELECTION);
    expect(parseSelection(ID)).toEqual({ kind: "one", id: ID });
  });

  it("degrades a token this build does not know rather than throwing", () => {
    /*
      `all` is the token PRD #742 will write, and this is the build that predates
      it. It parses as `One`, matches no row, and is therefore rendered as the
      local deck — with the substitution reported by `connection.selectionFallback`
      — exactly as `Selection`'s own deserializer degrades it Rust-side. The
      stored bytes are untouched, so the newer build's choice survives a save
      made from here.
    */
    const future = parseSelection("all");
    expect(future).toEqual({ kind: "one", id: "all" });
    expect(deckChoices(section(LOCAL_ENDPOINT_SELECTION)).find((choice) => sameSelection(choice.selection, future))).toBeUndefined();
    expect(selectionToken(future)).toBe("all");
  });

  it("compares by the stored token, so two readings of one deck are one deck", () => {
    expect(sameSelection(parseSelection(ID), { kind: "one", id: ID })).toBe(true);
    expect(sameSelection(LOCAL_DECK_SELECTION, parseSelection(ID))).toBe(false);
  });
});

describe("deckChoices", () => {
  it("always offers the local deck first, even with nothing configured", () => {
    // The local deck needs no configuration — `Endpoint::local()` resolves it
    // from the platform paths — so this list is never empty and the selector is
    // useful before anything is stored.
    expect(deckChoices(undefined)).toEqual([
      { token: LOCAL_ENDPOINT_SELECTION, selection: LOCAL_DECK_SELECTION, label: "This machine" },
    ]);
  });

  it("names each remote deck the way Rust describes it", () => {
    const labels = deckChoices(section(LOCAL_ENDPOINT_SELECTION)).map((choice) => choice.label);
    // `user@host`, with `:port` appended only when the port is not 22 — the same
    // derivation `RemoteEndpoint::describe()` performs, because there is no
    // stored display name to use instead.
    expect(labels).toEqual(["This machine", "vf@build-box.example.com", "relay.example.com:2222"]);
  });

  it("gives a deck with no host yet a label rather than an empty row", () => {
    const blank: EndpointSettingsDto = { remote: [{ host: "", id: ID, port: 22 }], selection: ID };
    expect(deckChoices(blank)[1].label).toBe("New deck");
  });
});

describe("describeEndpoint", () => {
  it("drops the default port and keeps any other", () => {
    expect(describeEndpoint({ host: "h", id: ID, port: 22 })).toBe("h");
    expect(describeEndpoint({ host: "h", id: ID, port: 2222 })).toBe("h:2222");
    expect(describeEndpoint({ host: "h", id: ID, port: 22, user: "u" })).toBe("u@h");
  });
});

describe("mintEndpointId", () => {
  it("is sixteen hex characters, which no reserved word can be", () => {
    const id = mintEndpointId();
    expect(id).toMatch(/^[0-9a-f]{16}$/);
    // Long enough and hex enough that it cannot collide with `local`, nor with a
    // word a future `Selection` variant would reserve.
    expect(id).not.toBe(LOCAL_ENDPOINT_SELECTION);
  });
});
